use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const MAX_CONTEXT_VALUE_BYTES: usize = 128;
const MAX_IDENTIFIER_BYTES: usize = 128;
pub const DEFAULT_ROTATION_BYTES: u64 = 20 * 1024 * 1024;
pub const DEFAULT_RETAINED_FILES: usize = 10;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ContextKey {
    Component,
    State,
    ErrorCode,
    Attempt,
    Listener,
}

impl ContextKey {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Component => "component",
            Self::State => "state",
            Self::ErrorCode => "error_code",
            Self::Attempt => "attempt",
            Self::Listener => "listener",
        }
    }
}

#[derive(Clone, Default, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SanitizedContext(BTreeMap<&'static str, String>);

impl SanitizedContext {
    #[must_use]
    pub const fn new() -> Self {
        Self(BTreeMap::new())
    }

    pub fn insert(&mut self, key: ContextKey, value: &str) -> Result<(), ContextError> {
        if is_sensitive(value) {
            return Err(ContextError);
        }
        self.0.insert(
            key.as_str(),
            truncate_utf8(value.trim(), MAX_CONTEXT_VALUE_BYTES),
        );
        Ok(())
    }

    #[must_use]
    pub fn encoded_len(&self) -> usize {
        serde_json::to_vec(self).map_or(0, |bytes| bytes.len())
    }
}

impl fmt::Debug for SanitizedContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("SanitizedContext")
            .field(&self.0)
            .finish()
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct ContextError;

impl ContextError {
    #[must_use]
    pub const fn code(self) -> &'static str {
        "unsafe_log_context"
    }
}

impl fmt::Debug for ContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl fmt::Display for ContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for ContextError {}

fn is_sensitive(value: &str) -> bool {
    let trimmed = value.trim();
    let lower = trimmed.to_ascii_lowercase();
    trimmed.is_empty()
        || trimmed.contains('@')
        || trimmed.starts_with('/')
        || trimmed.starts_with(r"\\")
        || trimmed.contains(r":\")
        || trimmed.contains(":/")
        || lower.contains("bearer ")
        || lower.contains("cookie")
        || lower.contains("csrf")
        || lower.contains("jwt")
        || lower.contains("token")
        || lower.contains("password")
        || lower.contains("credential")
        || trimmed.chars().any(char::is_control)
        || looks_like_jwt(trimmed)
}

fn looks_like_jwt(value: &str) -> bool {
    let segments: Vec<_> = value.split('.').collect();
    segments.len() == 3
        && segments
            .iter()
            .all(|segment| segment.len() >= 8 && segment.chars().all(is_base64url))
}

fn is_base64url(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Serialize)]
pub struct LogEvent {
    timestamp: String,
    level: LogLevel,
    event_code: String,
    version: &'static str,
    request_id: Option<String>,
    operation_id: Option<String>,
    context: SanitizedContext,
}

impl LogEvent {
    #[must_use]
    pub fn now(level: LogLevel, event_code: &str) -> Self {
        Self::at(OffsetDateTime::now_utc(), level, event_code)
    }

    #[must_use]
    pub fn at(timestamp: OffsetDateTime, level: LogLevel, event_code: &str) -> Self {
        Self {
            timestamp: timestamp
                .format(&Rfc3339)
                .unwrap_or_else(|_| "timestamp_unavailable".to_owned()),
            level,
            event_code: stable_identifier(event_code, "invalid_event_code"),
            version: env!("CARGO_PKG_VERSION"),
            request_id: None,
            operation_id: None,
            context: SanitizedContext::new(),
        }
    }

    #[must_use]
    pub fn with_request_id(mut self, request_id: &str) -> Self {
        self.request_id = safe_identifier(request_id);
        self
    }

    #[must_use]
    pub fn with_operation_id(mut self, operation_id: &str) -> Self {
        self.operation_id = safe_identifier(operation_id);
        self
    }

    #[must_use]
    pub fn with_context(mut self, context: SanitizedContext) -> Self {
        self.context = context;
        self
    }
}

fn stable_identifier(value: &str, fallback: &str) -> String {
    safe_identifier(value).unwrap_or_else(|| fallback.to_owned())
}

fn safe_identifier(value: &str) -> Option<String> {
    let valid = !value.is_empty()
        && value.len() <= MAX_IDENTIFIER_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
    valid.then(|| value.to_owned())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RotationPolicy {
    pub max_bytes: u64,
    pub retained_files: usize,
}

impl Default for RotationPolicy {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_ROTATION_BYTES,
            retained_files: DEFAULT_RETAINED_FILES,
        }
    }
}

pub struct JsonLogger {
    path: PathBuf,
    policy: RotationPolicy,
    file: Option<File>,
    bytes: u64,
}

impl JsonLogger {
    pub fn new(directory: &Path, policy: RotationPolicy) -> Result<Self, LogError> {
        if policy.max_bytes == 0 {
            return Err(LogError::OpenFailed);
        }
        fs::create_dir_all(directory).map_err(|_| LogError::OpenFailed)?;
        let path = directory.join("cellar.log");
        let file = open_append(&path)?;
        let bytes = file.metadata().map_err(|_| LogError::OpenFailed)?.len();
        Ok(Self {
            path,
            policy,
            file: Some(file),
            bytes,
        })
    }

    pub fn write(&mut self, event: &LogEvent) -> Result<(), LogError> {
        let mut line = Vec::new();
        write_json(&mut line, event)?;
        let line_len = u64::try_from(line.len()).map_err(|_| LogError::WriteFailed)?;
        if self.bytes > 0 && self.bytes.saturating_add(line_len) > self.policy.max_bytes {
            self.rotate()?;
        }
        let file = self.file.as_mut().ok_or(LogError::WriteFailed)?;
        file.write_all(&line).map_err(|_| LogError::WriteFailed)?;
        file.flush().map_err(|_| LogError::WriteFailed)?;
        self.bytes = self.bytes.saturating_add(line_len);
        Ok(())
    }

    fn rotate(&mut self) -> Result<(), LogError> {
        if let Some(file) = self.file.take() {
            file.sync_all().map_err(|_| LogError::RotationFailed)?;
        }
        if self.policy.retained_files == 0 {
            self.file = Some(File::create(&self.path).map_err(|_| LogError::RotationFailed)?);
            self.bytes = 0;
            return Ok(());
        }
        let oldest = rotated_path(&self.path, self.policy.retained_files);
        remove_if_present(&oldest)?;
        for index in (1..self.policy.retained_files).rev() {
            let source = rotated_path(&self.path, index);
            let destination = rotated_path(&self.path, index + 1);
            rename_if_present(&source, &destination)?;
        }
        fs::rename(&self.path, rotated_path(&self.path, 1))
            .map_err(|_| LogError::RotationFailed)?;
        self.file = Some(open_append(&self.path).map_err(|_| LogError::RotationFailed)?);
        self.bytes = 0;
        Ok(())
    }
}

fn open_append(path: &Path) -> Result<File, LogError> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|_| LogError::OpenFailed)
}

fn rotated_path(path: &Path, index: usize) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(format!(".{index}"));
    PathBuf::from(value)
}

fn remove_if_present(path: &Path) -> Result<(), LogError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(LogError::RotationFailed),
    }
}

fn rename_if_present(source: &Path, destination: &Path) -> Result<(), LogError> {
    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(LogError::RotationFailed),
    }
}

pub fn write_json(writer: &mut impl Write, event: &LogEvent) -> Result<(), LogError> {
    serde_json::to_writer(&mut *writer, event).map_err(|_| LogError::WriteFailed)?;
    writer.write_all(b"\n").map_err(|_| LogError::WriteFailed)
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum LogError {
    OpenFailed,
    WriteFailed,
    RotationFailed,
}

impl LogError {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::OpenFailed => "log_open_failed",
            Self::WriteFailed => "log_write_failed",
            Self::RotationFailed => "log_rotation_failed",
        }
    }
}

impl fmt::Debug for LogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl fmt::Display for LogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for LogError {}

#[derive(Serialize)]
pub struct FatalEvent {
    event_code: String,
    version: &'static str,
    context: SanitizedContext,
}

impl FatalEvent {
    #[must_use]
    pub fn new(event_code: &str, context: SanitizedContext) -> Self {
        Self {
            event_code: stable_identifier(event_code, "fatal_service_error"),
            version: env!("CARGO_PKG_VERSION"),
            context,
        }
    }

    fn message(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            r#"{"event_code":"fatal_service_error","version":"unknown","context":{}}"#.to_owned()
        })
    }
}

pub struct WindowsEventLog {
    source: String,
}

impl WindowsEventLog {
    #[must_use]
    pub fn new(source: &str) -> Self {
        Self {
            source: safe_identifier(source).unwrap_or_else(|| "Cellar".to_owned()),
        }
    }

    #[cfg(not(windows))]
    pub fn record(&self, _event: &FatalEvent) -> Result<(), EventLogError> {
        Err(EventLogError::UnsupportedPlatform)
    }

    #[cfg(windows)]
    pub fn record(&self, event: &FatalEvent) -> Result<(), EventLogError> {
        use std::os::windows::ffi::OsStrExt;
        use std::ptr;
        use windows_sys::Win32::System::EventLog::{
            DeregisterEventSource, EVENTLOG_ERROR_TYPE, RegisterEventSourceW, ReportEventW,
        };

        let source: Vec<u16> = std::ffi::OsStr::new(&self.source)
            .encode_wide()
            .chain(Some(0))
            .collect();
        let message: Vec<u16> = std::ffi::OsStr::new(&event.message())
            .encode_wide()
            .chain(Some(0))
            .collect();
        // SAFETY: `source` is a live NUL-terminated UTF-16 string.
        let handle = unsafe { RegisterEventSourceW(ptr::null(), source.as_ptr()) };
        if handle.is_null() {
            return Err(EventLogError::WriteFailed);
        }
        let strings = [message.as_ptr()];
        // SAFETY: the registered handle is live, and the single message
        // pointer targets a NUL-terminated UTF-16 buffer for the call.
        let reported = unsafe {
            ReportEventW(
                handle,
                EVENTLOG_ERROR_TYPE,
                0,
                0xC000_0001,
                ptr::null_mut(),
                1,
                0,
                strings.as_ptr(),
                ptr::null(),
            )
        };
        // SAFETY: `handle` was returned by RegisterEventSourceW and is closed once here.
        unsafe { DeregisterEventSource(handle) };
        if reported == 0 {
            Err(EventLogError::WriteFailed)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum EventLogError {
    UnsupportedPlatform,
    WriteFailed,
}

impl EventLogError {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::UnsupportedPlatform => "event_log_unsupported",
            Self::WriteFailed => "event_log_write_failed",
        }
    }
}

impl fmt::Debug for EventLogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl fmt::Display for EventLogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for EventLogError {}
