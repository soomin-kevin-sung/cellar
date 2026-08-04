//! Storage abstractions.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};
use std::time::SystemTime;

use tokio::fs;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use uuid::Uuid;

/// A single filename component validated for portable Windows storage.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SafeFileName(String);

impl SafeFileName {
    pub fn parse(value: impl AsRef<str>) -> Result<Self, InvalidFileName> {
        let value = value.as_ref();
        let reason = if value.is_empty() {
            Some(InvalidFileNameReason::Empty)
        } else if matches!(value, "." | "..") {
            Some(InvalidFileNameReason::DotComponent)
        } else if value.contains(['/', '\\']) {
            Some(InvalidFileNameReason::Separator)
        } else if value
            .chars()
            .any(|character| character <= '\u{1f}' || character == '\u{7f}')
        {
            Some(InvalidFileNameReason::ControlCharacter)
        } else if value
            .chars()
            .any(|character| "<>:\"|?*".contains(character))
        {
            Some(InvalidFileNameReason::InvalidCharacter)
        } else if value.ends_with(['.', ' ']) {
            Some(InvalidFileNameReason::TrailingDotOrSpace)
        } else if is_reserved_device_name(value) {
            Some(InvalidFileNameReason::ReservedDeviceName)
        } else if value.encode_utf16().count() > 255 {
            Some(InvalidFileNameReason::TooLong)
        } else {
            None
        };

        match reason {
            Some(reason) => Err(InvalidFileName { reason }),
            None => Ok(Self(value.to_owned())),
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn is_reserved_device_name(value: &str) -> bool {
    let base = value
        .split('.')
        .next()
        .unwrap_or(value)
        .trim_end_matches(' ');
    let upper = base.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$")
        || matches!(
            upper.strip_prefix("COM"),
            Some("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")
        )
        || matches!(
            upper.strip_prefix("LPT"),
            Some("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")
        )
}

impl fmt::Debug for SafeFileName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("SafeFileName")
            .field(&self.0)
            .finish()
    }
}

impl fmt::Display for SafeFileName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl TryFrom<&str> for SafeFileName {
    type Error = InvalidFileName;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl TryFrom<String> for SafeFileName {
    type Error = InvalidFileName;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidFileNameReason {
    Empty,
    DotComponent,
    Separator,
    InvalidCharacter,
    ControlCharacter,
    TrailingDotOrSpace,
    ReservedDeviceName,
    TooLong,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidFileName {
    reason: InvalidFileNameReason,
}

impl InvalidFileName {
    #[must_use]
    pub fn reason(&self) -> InvalidFileNameReason {
        self.reason
    }
}

impl fmt::Display for InvalidFileName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid file name: {:?}", self.reason)
    }
}

impl Error for InvalidFileName {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidRootReason {
    NotAbsolute,
    ParentTraversal,
}

pub enum StorageError {
    InvalidRoot(InvalidRootReason),
    AlreadyExists,
    NotFound,
    UnsafeManagedEntry,
    UnsafeEntry,
    NonEmptyStaging,
    InvalidBody,
    OffsetMismatch { expected: u64, actual: u64 },
    InsufficientSpace,
    ProjectCleanupFailed { source: io::Error },
    AmbiguousCleanup { source: io::Error },
    Io { source: io::Error },
}

impl fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRoot(reason) => write!(formatter, "invalid storage root: {reason:?}"),
            Self::AlreadyExists => formatter.write_str("storage entry already exists"),
            Self::NotFound => formatter.write_str("storage entry not found"),
            Self::UnsafeManagedEntry => formatter.write_str("unsafe managed storage entry"),
            Self::UnsafeEntry => formatter.write_str("unsafe or corrupt storage entry"),
            Self::NonEmptyStaging => formatter.write_str("staging file is not empty"),
            Self::InvalidBody => formatter.write_str("request body stream failed"),
            Self::OffsetMismatch { expected, actual } => write!(
                formatter,
                "staging offset mismatch: expected {expected}, actual {actual}"
            ),
            Self::InsufficientSpace => formatter.write_str("insufficient storage space"),
            Self::ProjectCleanupFailed { .. } => {
                formatter.write_str("project creation failed and cleanup may be incomplete")
            }
            Self::AmbiguousCleanup { .. } => {
                formatter.write_str("storage move completed but cleanup status is ambiguous")
            }
            Self::Io { .. } => formatter.write_str("storage I/O operation failed"),
        }
    }
}

impl fmt::Debug for StorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("StorageError")
            .field(&self.to_string())
            .finish()
    }
}

impl Error for StorageError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ProjectCleanupFailed { source }
            | Self::AmbiguousCleanup { source }
            | Self::Io { source } => Some(source),
            _ => None,
        }
    }
}

fn map_io(error: io::Error) -> StorageError {
    if matches!(
        error.kind(),
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded
    ) || is_no_space(&error)
    {
        StorageError::InsufficientSpace
    } else if is_platform_already_exists(&error) {
        StorageError::AlreadyExists
    } else {
        match error.kind() {
            io::ErrorKind::AlreadyExists => StorageError::AlreadyExists,
            io::ErrorKind::NotFound => StorageError::NotFound,
            _ => StorageError::Io { source: error },
        }
    }
}

#[cfg(windows)]
fn is_platform_already_exists(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(80 | 183))
}

#[cfg(not(windows))]
fn is_platform_already_exists(_error: &io::Error) -> bool {
    false
}

#[cfg(windows)]
fn is_no_space(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(39 | 112))
}

#[cfg(not(windows))]
fn is_no_space(error: &io::Error) -> bool {
    error.raw_os_error() == Some(28)
}

pub struct Storage {
    root: PathBuf,
    mutation_lock: Arc<tokio::sync::Mutex<()>>,
}

type MutationLock = tokio::sync::Mutex<()>;
type MutationLockRegistry = StdMutex<HashMap<PathBuf, Weak<MutationLock>>>;

static MUTATION_LOCKS: OnceLock<MutationLockRegistry> = OnceLock::new();

fn mutation_lock_for(root: &Path) -> Arc<MutationLock> {
    let registry = MUTATION_LOCKS.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut locks = registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(root).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(MutationLock::new(()));
    locks.insert(root.to_path_buf(), Arc::downgrade(&lock));
    lock
}

pub struct DiskFile {
    name: SafeFileName,
    size: u64,
    modified_at: SystemTime,
}

impl DiskFile {
    #[must_use]
    pub fn name(&self) -> &SafeFileName {
        &self.name
    }

    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    #[must_use]
    pub fn modified_at(&self) -> SystemTime {
        self.modified_at
    }
}

impl fmt::Debug for DiskFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiskFile")
            .field("name", &self.name)
            .field("size", &self.size)
            .field("modified_at", &self.modified_at)
            .finish()
    }
}

impl fmt::Debug for Storage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Storage { root: <redacted> }")
    }
}

impl Storage {
    pub fn new(data_root: PathBuf) -> Result<Self, StorageError> {
        if !data_root.is_absolute() {
            return Err(StorageError::InvalidRoot(InvalidRootReason::NotAbsolute));
        }
        if data_root
            .components()
            .any(|component| component == Component::ParentDir)
        {
            return Err(StorageError::InvalidRoot(
                InvalidRootReason::ParentTraversal,
            ));
        }
        let metadata = std::fs::symlink_metadata(&data_root).map_err(map_io)?;
        if !metadata.is_dir() || is_reparse_or_symlink(&metadata) {
            return Err(StorageError::UnsafeManagedEntry);
        }
        let data_root = std::fs::canonicalize(data_root).map_err(map_io)?;
        if !data_root.is_absolute() {
            return Err(StorageError::InvalidRoot(InvalidRootReason::NotAbsolute));
        }
        if data_root
            .components()
            .any(|component| component == Component::ParentDir)
        {
            return Err(StorageError::InvalidRoot(
                InvalidRootReason::ParentTraversal,
            ));
        }
        Ok(Self {
            mutation_lock: mutation_lock_for(&data_root),
            root: data_root,
        })
    }

    pub async fn initialize(&self) -> Result<(), StorageError> {
        let _guard = self.mutation_lock.lock().await;
        require_safe_directory(&self.root).await?;
        ensure_safe_directory(&self.projects_dir()).await?;
        ensure_safe_directory(&self.cellar_dir()).await?;
        ensure_safe_directory(&self.uploads_dir()).await?;
        Ok(())
    }

    pub async fn create_project_dir(&self, project_id: Uuid) -> Result<(), StorageError> {
        let _guard = self.mutation_lock.lock().await;
        self.require_projects_dir().await?;
        let project_dir = self.project_dir(project_id);
        if let Some(metadata) = optional_metadata(&project_dir).await? {
            if is_reparse_or_symlink(&metadata) || !metadata.is_dir() {
                return Err(StorageError::UnsafeManagedEntry);
            }
            return Err(StorageError::AlreadyExists);
        }

        fs::create_dir(&project_dir).await.map_err(map_io)?;
        let files_dir = project_dir.join("files");
        if let Err(error) = fs::create_dir(&files_dir).await {
            return Err(
                compensate_project_creation_failure(&project_dir, false, map_io(error)).await,
            );
        }
        if let Err(error) = require_safe_directory(&project_dir).await {
            return Err(compensate_project_creation_failure(&project_dir, true, error).await);
        }
        if let Err(error) = require_safe_directory(&files_dir).await {
            return Err(compensate_project_creation_failure(&project_dir, true, error).await);
        }
        Ok(())
    }

    pub async fn remove_empty_project_dir(&self, project_id: Uuid) -> Result<(), StorageError> {
        let _guard = self.mutation_lock.lock().await;
        self.require_projects_dir().await?;
        let project_dir = self.project_dir(project_id);
        let files_dir = project_dir.join("files");
        require_safe_directory(&project_dir).await?;
        require_safe_directory(&files_dir).await?;
        fs::remove_dir(&files_dir).await.map_err(map_io)?;
        fs::remove_dir(&project_dir).await.map_err(map_io)
    }

    /// Returns canonical UUID project directories that have Cellar's exact safe layout.
    ///
    /// Non-project names are ignored without traversal. A canonical UUID entry with an
    /// unexpected shape is managed-state corruption and stops the scan without changing it.
    pub async fn scan_project_directories(&self) -> Result<Vec<Uuid>, StorageError> {
        let _guard = self.mutation_lock.lock().await;
        self.require_projects_dir().await?;
        let mut directory = fs::read_dir(self.projects_dir()).await.map_err(map_io)?;
        let mut project_ids = Vec::new();
        while let Some(entry) = directory.next_entry().await.map_err(map_io)? {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(project_id) = Uuid::parse_str(&name) else {
                continue;
            };
            if project_id.to_string() != name {
                continue;
            }
            require_safe_project_shape(&entry.path()).await?;
            project_ids.push(project_id);
        }
        project_ids.sort_unstable();
        Ok(project_ids)
    }

    pub async fn create_staging(&self, upload_id: Uuid) -> Result<(), StorageError> {
        let _guard = self.mutation_lock.lock().await;
        self.require_uploads_dir().await?;
        let path = self.staging_path(upload_id);
        if let Some(metadata) = optional_metadata(&path).await? {
            if is_reparse_or_symlink(&metadata) || !metadata.is_file() {
                return Err(StorageError::UnsafeManagedEntry);
            }
            return Err(StorageError::AlreadyExists);
        }
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .await
            .map(|_| ())
            .map_err(map_io)
    }

    /// Removes the exact UUID staging file. A missing file is an idempotent success.
    pub async fn remove_staging(&self, upload_id: Uuid) -> Result<(), StorageError> {
        let _guard = self.mutation_lock.lock().await;
        self.require_uploads_dir().await?;
        let path = self.staging_path(upload_id);
        let Some(metadata) = optional_metadata(&path).await? else {
            return Ok(());
        };
        require_safe_regular_file_metadata(&metadata)?;
        match fs::remove_file(path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(map_io(error)),
        }
    }

    /// Removes one exact UUID staging file only when it is a safe empty regular file.
    /// A file absent before inspection is an idempotent success.
    pub async fn remove_empty_staging(&self, upload_id: Uuid) -> Result<(), StorageError> {
        let _guard = self.mutation_lock.lock().await;
        self.require_uploads_dir().await?;
        let path = self.staging_path(upload_id);
        let Some(metadata) = optional_metadata(&path).await? else {
            return Ok(());
        };
        require_safe_regular_file_metadata(&metadata)?;
        if metadata.len() != 0 {
            return Err(StorageError::NonEmptyStaging);
        }
        match fs::remove_file(path).await {
            Ok(()) => Ok(()),
            Err(source) => Err(StorageError::AmbiguousCleanup { source }),
        }
    }

    pub async fn staging_len(&self, upload_id: Uuid) -> Result<Option<u64>, StorageError> {
        self.require_uploads_dir().await?;
        let Some(metadata) = optional_metadata(&self.staging_path(upload_id)).await? else {
            return Ok(None);
        };
        require_safe_exact_entry_metadata(&metadata)?;
        Ok(Some(metadata.len()))
    }

    pub async fn write_chunk<R>(
        &self,
        upload_id: Uuid,
        offset: u64,
        mut reader: R,
    ) -> Result<u64, StorageError>
    where
        R: AsyncRead + Unpin,
    {
        let _guard = self.mutation_lock.lock().await;
        let mut file = self.open_staging_for_write(upload_id).await?;
        let metadata = file.metadata().await.map_err(map_io)?;
        require_safe_regular_file_metadata(&metadata)?;
        let actual = metadata.len();
        if actual != offset {
            return Err(StorageError::OffsetMismatch {
                expected: offset,
                actual,
            });
        }
        file.seek(io::SeekFrom::Start(offset))
            .await
            .map_err(map_io)?;
        let mut written = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = reader
                .read(&mut buffer)
                .await
                .map_err(|_| StorageError::InvalidBody)?;
            if read == 0 {
                break;
            }
            file.write_all(&buffer[..read]).await.map_err(map_io)?;
            written = written
                .checked_add(read as u64)
                .ok_or(StorageError::InvalidBody)?;
        }
        file.sync_data().await.map_err(map_io)?;
        Ok(written)
    }

    pub async fn sync_staging(&self, upload_id: Uuid) -> Result<(), StorageError> {
        let _guard = self.mutation_lock.lock().await;
        self.open_staging_for_write(upload_id)
            .await?
            .sync_all()
            .await
            .map_err(map_io)
    }

    pub async fn truncate_staging(&self, upload_id: Uuid, len: u64) -> Result<(), StorageError> {
        let _guard = self.mutation_lock.lock().await;
        let file = self.open_staging_for_write(upload_id).await?;
        file.set_len(len).await.map_err(map_io)?;
        file.sync_data().await.map_err(map_io)
    }

    pub async fn destination_exists(
        &self,
        project_id: Uuid,
        name: &SafeFileName,
    ) -> Result<bool, StorageError> {
        let files_dir = self.require_project_files_dir(project_id).await?;
        let Some(metadata) = optional_metadata(&files_dir.join(name.as_str())).await? else {
            return Ok(false);
        };
        require_safe_regular_file_metadata(&metadata)?;
        Ok(true)
    }

    pub async fn finalize_no_replace(
        &self,
        upload_id: Uuid,
        project_id: Uuid,
        name: &SafeFileName,
    ) -> Result<(), StorageError> {
        let _guard = self.mutation_lock.lock().await;
        self.require_uploads_dir().await?;
        let source = self.staging_path(upload_id);
        let source_metadata = fs::symlink_metadata(&source).await.map_err(map_io)?;
        require_safe_exact_entry_metadata(&source_metadata)?;

        let files_dir = self.require_project_files_dir(project_id).await?;
        let destination = files_dir.join(name.as_str());
        if let Some(metadata) = optional_metadata(&destination).await? {
            require_safe_exact_entry_metadata(&metadata)?;
        }

        atomic_move_no_replace(source, destination).await
    }

    pub async fn final_file_len(
        &self,
        project_id: Uuid,
        name: &SafeFileName,
    ) -> Result<Option<u64>, StorageError> {
        let files_dir = self.require_project_files_dir(project_id).await?;
        let Some(metadata) = optional_metadata(&files_dir.join(name.as_str())).await? else {
            return Ok(None);
        };
        require_safe_exact_entry_metadata(&metadata)?;
        Ok(Some(metadata.len()))
    }

    /// Opens one validated final file for streaming without exposing its host path.
    pub async fn open_final_file(
        &self,
        project_id: Uuid,
        name: &SafeFileName,
    ) -> Result<fs::File, StorageError> {
        let files_dir = self
            .require_project_files_dir(project_id)
            .await
            .map_err(|error| match error {
                StorageError::NotFound => StorageError::UnsafeManagedEntry,
                error => error,
            })?;
        let path = files_dir.join(name.as_str());
        let metadata = fs::symlink_metadata(&path).await.map_err(map_io)?;
        require_safe_exact_entry_metadata(&metadata)?;
        let file = open_final_leaf_no_follow(path).await?;
        let metadata = file.metadata().await.map_err(map_io)?;
        require_safe_regular_file_metadata(&metadata)?;
        Ok(file)
    }

    pub async fn list_files(&self, project_id: Uuid) -> Result<Vec<DiskFile>, StorageError> {
        let files_dir = self.require_project_files_dir(project_id).await?;
        let mut directory = fs::read_dir(files_dir).await.map_err(map_io)?;
        let mut files = Vec::new();
        while let Some(entry) = directory.next_entry().await.map_err(map_io)? {
            let metadata = fs::symlink_metadata(entry.path()).await.map_err(map_io)?;
            if !metadata.is_file() || is_reparse_or_symlink(&metadata) {
                continue;
            }
            let raw_name = entry.file_name().into_string().ok();
            let Some(name) = raw_name.and_then(|name| SafeFileName::parse(name).ok()) else {
                continue;
            };
            let modified_at = metadata.modified().map_err(map_io)?;
            files.push(DiskFile {
                name,
                size: metadata.len(),
                modified_at,
            });
        }
        Ok(files)
    }

    fn projects_dir(&self) -> PathBuf {
        self.root.join("projects")
    }

    fn cellar_dir(&self) -> PathBuf {
        self.root.join(".cellar")
    }

    fn uploads_dir(&self) -> PathBuf {
        self.cellar_dir().join("uploads")
    }

    fn project_dir(&self, project_id: Uuid) -> PathBuf {
        self.projects_dir().join(project_id.to_string())
    }

    fn staging_path(&self, upload_id: Uuid) -> PathBuf {
        self.uploads_dir().join(format!("{upload_id}.part"))
    }

    async fn require_projects_dir(&self) -> Result<(), StorageError> {
        require_safe_directory(&self.root).await?;
        require_safe_directory(&self.projects_dir()).await
    }

    async fn require_uploads_dir(&self) -> Result<(), StorageError> {
        require_safe_directory(&self.root).await?;
        require_safe_directory(&self.cellar_dir()).await?;
        require_safe_directory(&self.uploads_dir()).await
    }

    async fn require_project_files_dir(&self, project_id: Uuid) -> Result<PathBuf, StorageError> {
        self.require_projects_dir().await?;
        let project_dir = self.project_dir(project_id);
        require_safe_directory(&project_dir).await?;
        let files_dir = project_dir.join("files");
        require_safe_directory(&files_dir).await?;
        Ok(files_dir)
    }

    async fn open_staging_for_write(&self, upload_id: Uuid) -> Result<fs::File, StorageError> {
        self.require_uploads_dir().await?;
        let path = self.staging_path(upload_id);
        let metadata = fs::symlink_metadata(&path).await.map_err(map_io)?;
        require_safe_exact_entry_metadata(&metadata)?;
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .await
            .map_err(map_io)
    }
}

async fn open_final_leaf_no_follow(path: PathBuf) -> Result<fs::File, StorageError> {
    final_leaf_open_options().open(path).await.map_err(map_io)
}

fn final_leaf_open_options() -> fs::OpenOptions {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    #[cfg(windows)]
    options.custom_flags(final_leaf_custom_flags());
    options
}

#[cfg(windows)]
const fn final_leaf_custom_flags() -> u32 {
    windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT
}

#[cfg(windows)]
async fn atomic_move_no_replace(source: PathBuf, destination: PathBuf) -> Result<(), StorageError> {
    tokio::task::spawn_blocking(move || {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

        let source = source
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let destination = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        // SAFETY: Both pointers refer to live, NUL-terminated UTF-16 buffers for
        // the duration of the call. Omitting REPLACE_EXISTING is intentional.
        let result = unsafe {
            MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_WRITE_THROUGH,
            )
        };
        if result == 0 {
            Err(map_io(io::Error::last_os_error()))
        } else {
            Ok(())
        }
    })
    .await
    .map_err(|error| map_io(io::Error::other(error)))?
}

#[cfg(not(windows))]
async fn atomic_move_no_replace(source: PathBuf, destination: PathBuf) -> Result<(), StorageError> {
    let _ = (source, destination);
    Err(StorageError::Io {
        source: io::Error::new(
            io::ErrorKind::Unsupported,
            "atomic upload publication is unsupported on this platform",
        ),
    })
}

async fn optional_metadata(path: &Path) -> Result<Option<std::fs::Metadata>, StorageError> {
    match fs::symlink_metadata(path).await {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(map_io(error)),
    }
}

async fn compensate_project_creation_failure(
    project_dir: &Path,
    files_created: bool,
    original: StorageError,
) -> StorageError {
    match cleanup_project_creation(project_dir, files_created).await {
        Ok(()) => original,
        Err(source) => StorageError::ProjectCleanupFailed { source },
    }
}

async fn cleanup_project_creation(project_dir: &Path, files_created: bool) -> io::Result<()> {
    let mut first_error = None;
    if files_created
        && let Err(error) = fs::remove_dir(project_dir.join("files")).await
        && error.kind() != io::ErrorKind::NotFound
    {
        first_error = Some(error);
    }
    if let Err(error) = fs::remove_dir(project_dir).await
        && error.kind() != io::ErrorKind::NotFound
        && first_error.is_none()
    {
        first_error = Some(error);
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn require_safe_regular_file_metadata(metadata: &std::fs::Metadata) -> Result<(), StorageError> {
    if !metadata.is_file() || is_reparse_or_symlink(metadata) {
        return Err(StorageError::UnsafeManagedEntry);
    }
    Ok(())
}

fn require_safe_exact_entry_metadata(metadata: &std::fs::Metadata) -> Result<(), StorageError> {
    if !metadata.is_file() || is_reparse_or_symlink(metadata) {
        return Err(StorageError::UnsafeEntry);
    }
    Ok(())
}

async fn ensure_safe_directory(path: &Path) -> Result<(), StorageError> {
    match fs::create_dir(path).await {
        Ok(()) => require_safe_directory(path).await,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            require_safe_directory(path).await
        }
        Err(error) => Err(map_io(error)),
    }
}

async fn require_safe_directory(path: &Path) -> Result<(), StorageError> {
    let metadata = fs::symlink_metadata(path).await.map_err(map_io)?;
    if !metadata.is_dir() || is_reparse_or_symlink(&metadata) {
        return Err(StorageError::UnsafeManagedEntry);
    }
    Ok(())
}

async fn require_safe_project_shape(project_dir: &Path) -> Result<(), StorageError> {
    require_safe_directory(project_dir).await?;
    let mut entries = fs::read_dir(project_dir).await.map_err(map_io)?;
    let Some(entry) = entries.next_entry().await.map_err(map_io)? else {
        return Err(StorageError::UnsafeManagedEntry);
    };
    if entry.file_name() != "files" || entries.next_entry().await.map_err(map_io)?.is_some() {
        return Err(StorageError::UnsafeManagedEntry);
    }
    require_safe_directory(&entry.path())
        .await
        .map_err(|_| StorageError::UnsafeManagedEntry)
}

#[cfg(windows)]
fn is_reparse_or_symlink(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_or_symlink(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::SystemTime;

    use super::*;
    use tempfile::{TempDir, tempdir};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[cfg(windows)]
    #[test]
    fn final_leaf_open_flags_are_reparse_safe_and_non_destructive() {
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_DELETE_ON_CLOSE, FILE_FLAG_OPEN_REPARSE_POINT,
        };

        let flags = final_leaf_custom_flags();
        assert_eq!(
            flags & FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_FLAG_OPEN_REPARSE_POINT
        );
        assert_eq!(flags & FILE_FLAG_DELETE_ON_CLOSE, 0);
        assert_eq!(flags, FILE_FLAG_OPEN_REPARSE_POINT);
    }

    #[test]
    fn safe_file_name_accepts_and_preserves_exact_input() {
        for value in [
            "report 2026.pdf",
            "Résumé.PDF",
            "資料.txt",
            "COM0",
            "COM10",
            "LPT4x",
        ] {
            let name = SafeFileName::parse(value).expect("valid file name");
            assert_eq!(name.as_str(), value);
        }
    }

    #[test]
    fn safe_file_name_rejects_unsafe_windows_names() {
        let cases = [
            ("", InvalidFileNameReason::Empty),
            (".", InvalidFileNameReason::DotComponent),
            ("..", InvalidFileNameReason::DotComponent),
            ("a/b", InvalidFileNameReason::Separator),
            ("a\\b", InvalidFileNameReason::Separator),
            ("a:b", InvalidFileNameReason::InvalidCharacter),
            ("trailing.", InvalidFileNameReason::TrailingDotOrSpace),
            ("trailing ", InvalidFileNameReason::TrailingDotOrSpace),
            ("CON", InvalidFileNameReason::ReservedDeviceName),
            ("con.txt", InvalidFileNameReason::ReservedDeviceName),
            ("CON .txt", InvalidFileNameReason::ReservedDeviceName),
            ("PRN", InvalidFileNameReason::ReservedDeviceName),
            ("AUX", InvalidFileNameReason::ReservedDeviceName),
            ("NUL", InvalidFileNameReason::ReservedDeviceName),
            ("COM1", InvalidFileNameReason::ReservedDeviceName),
            ("COM¹", InvalidFileNameReason::ReservedDeviceName),
            ("com².txt", InvalidFileNameReason::ReservedDeviceName),
            ("LPT9", InvalidFileNameReason::ReservedDeviceName),
            ("LPT³.archive", InvalidFileNameReason::ReservedDeviceName),
            ("CLOCK$", InvalidFileNameReason::ReservedDeviceName),
            ("control\u{1f}.txt", InvalidFileNameReason::ControlCharacter),
            ("delete\u{7f}.txt", InvalidFileNameReason::ControlCharacter),
        ];

        for (value, expected) in cases {
            let error = SafeFileName::parse(value).expect_err(value);
            assert_eq!(error.reason(), expected, "input: {value:?}");
        }
    }

    #[test]
    fn safe_file_name_counts_utf16_code_units() {
        let boundary = "😀".repeat(127) + "a";
        assert_eq!(boundary.encode_utf16().count(), 255);
        assert!(SafeFileName::parse(&boundary).is_ok());

        let too_long = "😀".repeat(128);
        assert_eq!(too_long.encode_utf16().count(), 256);
        assert_eq!(
            SafeFileName::parse(&too_long).unwrap_err().reason(),
            InvalidFileNameReason::TooLong
        );
    }

    #[test]
    fn storage_new_rejects_relative_and_parent_traversal_roots_without_creating() {
        assert!(matches!(
            Storage::new(PathBuf::from("relative-root")),
            Err(StorageError::InvalidRoot(InvalidRootReason::NotAbsolute))
        ));

        let temp = tempdir().unwrap();
        let traversing = temp.path().join("missing").join("..").join("root");
        assert!(matches!(
            Storage::new(traversing),
            Err(StorageError::InvalidRoot(
                InvalidRootReason::ParentTraversal
            ))
        ));
        assert!(!temp.path().join("missing").exists());
        assert!(!Path::new("relative-root").exists());
    }

    #[tokio::test]
    async fn initialize_creates_exact_layout_and_is_idempotent() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let storage = Storage::new(root.clone()).unwrap();

        storage.initialize().await.unwrap();
        storage.initialize().await.unwrap();

        assert!(root.join("projects").is_dir());
        assert!(root.join(".cellar").join("uploads").is_dir());
        let mut entries = std::fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries.sort();
        assert_eq!(entries, [".cellar", "projects"]);
    }

    #[test]
    fn storage_new_rejects_missing_root_without_creating_it() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("missing");

        assert!(matches!(
            Storage::new(root.clone()),
            Err(StorageError::NotFound)
        ));
        assert!(!root.exists());
    }

    #[test]
    fn storage_new_rejects_non_directory_root() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("file-root");
        std::fs::write(&root, b"not a directory").unwrap();

        assert!(matches!(
            Storage::new(root),
            Err(StorageError::UnsafeManagedEntry)
        ));
    }

    #[test]
    fn storage_new_rejects_symlink_root_when_supported() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("target-root");
        let link = temp.path().join("link-root");
        std::fs::create_dir(&target).unwrap();
        if !try_symlink_dir(&target, &link) {
            return;
        }

        assert!(matches!(
            Storage::new(link),
            Err(StorageError::UnsafeManagedEntry)
        ));
    }

    #[tokio::test]
    async fn initialize_rejects_non_directory_managed_entry() {
        let temp = tempdir().unwrap();
        let root = existing_root(&temp);
        std::fs::write(root.join("projects"), b"unsafe").unwrap();
        let storage = Storage::new(root).unwrap();

        assert!(matches!(
            storage.initialize().await,
            Err(StorageError::UnsafeManagedEntry)
        ));
    }

    #[tokio::test]
    async fn initialize_rejects_symlink_at_managed_directory_when_supported() {
        let temp = tempdir().unwrap();
        let root = existing_root(&temp);
        let target = temp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        if !try_symlink_dir(&target, &root.join("projects")) {
            return;
        }
        let storage = Storage::new(root).unwrap();

        assert!(matches!(
            storage.initialize().await,
            Err(StorageError::UnsafeManagedEntry)
        ));
    }

    #[tokio::test]
    async fn project_directories_use_uuid_and_create_new_semantics() {
        let temp = tempdir().unwrap();
        let root = existing_root(&temp);
        let storage = Storage::new(root.clone()).unwrap();
        storage.initialize().await.unwrap();
        let project_id = Uuid::parse_str("018f1010-7b2a-7000-8000-000000000001").unwrap();

        storage.create_project_dir(project_id).await.unwrap();
        assert!(
            root.join("projects")
                .join(project_id.to_string())
                .join("files")
                .is_dir()
        );
        assert!(matches!(
            storage.create_project_dir(project_id).await,
            Err(StorageError::AlreadyExists)
        ));
    }

    #[tokio::test]
    async fn project_directory_scan_returns_only_immediate_canonical_safe_uuid_layouts() {
        let temp = tempdir().unwrap();
        let root = existing_root(&temp);
        let storage = Storage::new(root.clone()).unwrap();
        storage.initialize().await.unwrap();
        let first = Uuid::parse_str("018f1010-7b2a-7000-8000-000000000010").unwrap();
        let second = Uuid::parse_str("018f1010-7b2a-7000-8000-000000000011").unwrap();
        storage.create_project_dir(second).await.unwrap();
        storage.create_project_dir(first).await.unwrap();
        std::fs::create_dir(root.join("projects").join("not-a-project")).unwrap();
        for noncanonical in [
            "018F1010-7B2A-7000-8000-000000000013",
            "018f10107b2a70008000000000000014",
        ] {
            let directory = root.join("projects").join(noncanonical);
            std::fs::create_dir(&directory).unwrap();
            std::fs::create_dir(directory.join("files")).unwrap();
        }
        std::fs::create_dir(
            root.join("projects")
                .join(first.to_string())
                .join("files")
                .join("nested-uuid-does-not-count"),
        )
        .unwrap();

        assert_eq!(
            storage.scan_project_directories().await.unwrap(),
            vec![first, second]
        );
    }

    #[tokio::test]
    async fn project_directory_scan_rejects_canonical_uuid_with_unexpected_or_unsafe_shape() {
        for mutation in [
            "missing_files",
            "extra_entry",
            "linked_files",
            "linked_project",
        ] {
            let temp = tempdir().unwrap();
            let root = existing_root(&temp);
            let storage = Storage::new(root.clone()).unwrap();
            storage.initialize().await.unwrap();
            let project_id = Uuid::parse_str("018f1010-7b2a-7000-8000-000000000012").unwrap();
            storage.create_project_dir(project_id).await.unwrap();
            let project = root.join("projects").join(project_id.to_string());
            match mutation {
                "missing_files" => std::fs::remove_dir(project.join("files")).unwrap(),
                "extra_entry" => std::fs::write(project.join("unexpected"), b"unsafe").unwrap(),
                "linked_files" => {
                    std::fs::remove_dir(project.join("files")).unwrap();
                    let target = temp.path().join("outside");
                    std::fs::create_dir(&target).unwrap();
                    if !try_symlink_dir(&target, &project.join("files")) {
                        continue;
                    }
                }
                "linked_project" => {
                    std::fs::remove_dir(project.join("files")).unwrap();
                    std::fs::remove_dir(&project).unwrap();
                    let target = temp.path().join("outside");
                    std::fs::create_dir(&target).unwrap();
                    std::fs::create_dir(target.join("files")).unwrap();
                    if !try_symlink_dir(&target, &project) {
                        continue;
                    }
                }
                _ => unreachable!(),
            }

            assert!(matches!(
                storage.scan_project_directories().await,
                Err(StorageError::UnsafeManagedEntry)
            ));
            assert!(project.exists(), "audit must preserve {mutation}");
        }
    }

    #[tokio::test]
    async fn remove_project_dir_only_removes_empty_exact_directories() {
        let temp = tempdir().unwrap();
        let root = existing_root(&temp);
        let storage = Storage::new(root.clone()).unwrap();
        storage.initialize().await.unwrap();
        let project_id = Uuid::parse_str("018f1010-7b2a-7000-8000-000000000002").unwrap();
        storage.create_project_dir(project_id).await.unwrap();
        let project = root.join("projects").join(project_id.to_string());
        std::fs::write(project.join("files").join("keep.txt"), b"keep").unwrap();

        assert!(storage.remove_empty_project_dir(project_id).await.is_err());
        assert_eq!(
            std::fs::read(project.join("files").join("keep.txt")).unwrap(),
            b"keep"
        );
        std::fs::remove_file(project.join("files").join("keep.txt")).unwrap();
        storage.remove_empty_project_dir(project_id).await.unwrap();
        assert!(!project.exists());
    }

    #[tokio::test]
    async fn project_creation_cleanup_removes_only_exact_empty_directories() {
        let temp = tempdir().unwrap();
        let project = temp.path().join("project");
        let files = project.join("files");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&files).unwrap();

        let error =
            compensate_project_creation_failure(&project, true, StorageError::UnsafeManagedEntry)
                .await;

        assert!(matches!(error, StorageError::UnsafeManagedEntry));
        assert!(!project.exists());
    }

    #[tokio::test]
    async fn project_creation_cleanup_surfaces_nonempty_failure_without_recursive_deletion() {
        let temp = tempdir().unwrap();
        let project = temp.path().join("project");
        let files = project.join("files");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&files).unwrap();
        std::fs::write(files.join("keep.txt"), b"keep").unwrap();

        let error =
            compensate_project_creation_failure(&project, true, StorageError::UnsafeManagedEntry)
                .await;

        assert!(matches!(error, StorageError::ProjectCleanupFailed { .. }));
        assert_eq!(std::fs::read(files.join("keep.txt")).unwrap(), b"keep");
    }

    #[tokio::test]
    async fn staging_uses_exact_part_path_and_missing_remove_is_idempotent() {
        let temp = tempdir().unwrap();
        let root = existing_root(&temp);
        let storage = Storage::new(root.clone()).unwrap();
        storage.initialize().await.unwrap();
        let upload_id = Uuid::parse_str("018f1010-7b2a-7000-8000-000000000003").unwrap();
        let part = root
            .join(".cellar")
            .join("uploads")
            .join(format!("{upload_id}.part"));

        assert_eq!(storage.staging_len(upload_id).await.unwrap(), None);
        storage.create_staging(upload_id).await.unwrap();
        assert!(part.is_file());
        assert_eq!(storage.staging_len(upload_id).await.unwrap(), Some(0));
        assert!(matches!(
            storage.create_staging(upload_id).await,
            Err(StorageError::AlreadyExists)
        ));
        storage.remove_staging(upload_id).await.unwrap();
        storage.remove_staging(upload_id).await.unwrap();
        assert!(!part.exists());
    }

    #[tokio::test]
    async fn empty_staging_compensation_removes_only_exact_file_and_missing_is_idempotent() {
        let temp = tempdir().unwrap();
        let root = existing_root(&temp);
        let storage = Storage::new(root.clone()).unwrap();
        storage.initialize().await.unwrap();
        let upload_id = Uuid::parse_str("018f1010-7b2a-7000-8000-000000000103").unwrap();
        let sibling = root.join(".cellar").join("uploads").join("keep.part");
        std::fs::write(&sibling, b"keep").unwrap();

        storage.create_staging(upload_id).await.unwrap();
        storage.remove_empty_staging(upload_id).await.unwrap();
        storage.remove_empty_staging(upload_id).await.unwrap();

        assert_eq!(storage.staging_len(upload_id).await.unwrap(), None);
        assert_eq!(std::fs::read(sibling).unwrap(), b"keep");
    }

    #[tokio::test]
    async fn empty_staging_compensation_refuses_and_preserves_nonempty_file() {
        let temp = tempdir().unwrap();
        let storage = Storage::new(existing_root(&temp)).unwrap();
        storage.initialize().await.unwrap();
        let upload_id = Uuid::parse_str("018f1010-7b2a-7000-8000-000000000104").unwrap();
        storage.create_staging(upload_id).await.unwrap();
        storage
            .write_chunk(upload_id, 0, &b"important"[..])
            .await
            .unwrap();

        assert!(matches!(
            storage.remove_empty_staging(upload_id).await,
            Err(StorageError::NonEmptyStaging)
        ));
        assert_eq!(storage.staging_len(upload_id).await.unwrap(), Some(9));
    }

    #[tokio::test]
    async fn create_staging_rejects_symlink_entry_when_supported() {
        let temp = tempdir().unwrap();
        let root = existing_root(&temp);
        let storage = Storage::new(root.clone()).unwrap();
        storage.initialize().await.unwrap();
        let upload_id = Uuid::parse_str("018f1010-7b2a-7000-8000-00000000000d").unwrap();
        let target = root.join("target.part");
        std::fs::write(&target, b"target").unwrap();
        let link = root
            .join(".cellar")
            .join("uploads")
            .join(format!("{upload_id}.part"));
        if !try_symlink_file(&target, &link) {
            return;
        }

        assert!(matches!(
            storage.create_staging(upload_id).await,
            Err(StorageError::UnsafeManagedEntry)
        ));
        assert_eq!(std::fs::read(target).unwrap(), b"target");
    }

    #[tokio::test]
    async fn write_chunk_streams_at_exact_expected_offset() {
        let temp = tempdir().unwrap();
        let root = existing_root(&temp);
        let storage = Storage::new(root.clone()).unwrap();
        storage.initialize().await.unwrap();
        let upload_id = Uuid::parse_str("018f1010-7b2a-7000-8000-000000000004").unwrap();
        storage.create_staging(upload_id).await.unwrap();

        let (mut producer, consumer) = tokio::io::duplex(7);
        let producing = tokio::spawn(async move {
            producer.write_all(b"streamed chunk").await.unwrap();
        });
        let written = storage.write_chunk(upload_id, 0, consumer).await.unwrap();
        producing.await.unwrap();
        assert_eq!(written, 14);

        let part = root
            .join(".cellar")
            .join("uploads")
            .join(format!("{upload_id}.part"));
        assert_eq!(tokio::fs::read(part).await.unwrap(), b"streamed chunk");
    }

    #[tokio::test]
    async fn write_chunk_rejects_offset_mismatch_without_changing_bytes() {
        let temp = tempdir().unwrap();
        let storage = Storage::new(existing_root(&temp)).unwrap();
        storage.initialize().await.unwrap();
        let upload_id = Uuid::parse_str("018f1010-7b2a-7000-8000-000000000005").unwrap();
        storage.create_staging(upload_id).await.unwrap();
        storage
            .write_chunk(upload_id, 0, &b"abc"[..])
            .await
            .unwrap();

        let error = storage
            .write_chunk(upload_id, 2, &b"X"[..])
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            StorageError::OffsetMismatch {
                expected: 2,
                actual: 3
            }
        ));
        assert_eq!(storage.staging_len(upload_id).await.unwrap(), Some(3));
    }

    #[tokio::test]
    async fn independent_storage_instances_serialize_writes_for_same_root() {
        let temp = tempdir().unwrap();
        let root = existing_root(&temp);
        let storage_a = Storage::new(root.clone()).unwrap();
        let storage_b = Storage::new(root_alias(&root)).unwrap();
        assert!(std::sync::Arc::ptr_eq(
            &storage_a.mutation_lock,
            &storage_b.mutation_lock
        ));
        storage_a.initialize().await.unwrap();
        let upload_id = Uuid::parse_str("018f1010-7b2a-7000-8000-00000000000e").unwrap();
        storage_a.create_staging(upload_id).await.unwrap();
        let input_a = vec![b'A'; 32 * 1024];
        let input_b = vec![b'B'; 32 * 1024];
        let (mut producer_a, consumer_a) = tokio::io::duplex(7);
        let (mut producer_b, consumer_b) = tokio::io::duplex(7);
        let producing_a = tokio::spawn({
            let input_a = input_a.clone();
            async move {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                producer_a.write_all(&input_a).await
            }
        });
        let producing_b = tokio::spawn({
            let input_b = input_b.clone();
            async move {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                producer_b.write_all(&input_b).await
            }
        });
        let write_a =
            tokio::spawn(async move { storage_a.write_chunk(upload_id, 0, consumer_a).await });
        let write_b =
            tokio::spawn(async move { storage_b.write_chunk(upload_id, 0, consumer_b).await });

        let results = [write_a.await.unwrap(), write_b.await.unwrap()];
        let _ = producing_a.await.unwrap();
        let _ = producing_b.await.unwrap();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(StorageError::OffsetMismatch { .. })))
                .count(),
            1
        );
        let bytes = std::fs::read(
            root.join(".cellar")
                .join("uploads")
                .join(format!("{upload_id}.part")),
        )
        .unwrap();
        assert!(bytes == input_a || bytes == input_b);
    }

    #[tokio::test]
    async fn truncate_staging_sets_exact_length_and_can_be_synced() {
        let temp = tempdir().unwrap();
        let storage = Storage::new(existing_root(&temp)).unwrap();
        storage.initialize().await.unwrap();
        let upload_id = Uuid::parse_str("018f1010-7b2a-7000-8000-000000000006").unwrap();
        storage.create_staging(upload_id).await.unwrap();
        storage
            .write_chunk(upload_id, 0, &b"abcdef"[..])
            .await
            .unwrap();

        storage.truncate_staging(upload_id, 3).await.unwrap();
        storage.sync_staging(upload_id).await.unwrap();
        assert_eq!(storage.staging_len(upload_id).await.unwrap(), Some(3));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn finalize_no_replace_moves_exact_staging_bytes() {
        let temp = tempdir().unwrap();
        let root = existing_root(&temp);
        let storage = Storage::new(root.clone()).unwrap();
        storage.initialize().await.unwrap();
        let project_id = Uuid::parse_str("018f1010-7b2a-7000-8000-000000000007").unwrap();
        let upload_id = Uuid::parse_str("018f1010-7b2a-7000-8000-000000000008").unwrap();
        let name = SafeFileName::parse("Report Final.pdf").unwrap();
        storage.create_project_dir(project_id).await.unwrap();
        storage.create_staging(upload_id).await.unwrap();
        storage
            .write_chunk(upload_id, 0, &b"final bytes"[..])
            .await
            .unwrap();

        storage
            .finalize_no_replace(upload_id, project_id, &name)
            .await
            .unwrap();

        assert_eq!(storage.staging_len(upload_id).await.unwrap(), None);
        assert_eq!(
            std::fs::read(
                root.join("projects")
                    .join(project_id.to_string())
                    .join("files")
                    .join(name.as_str())
            )
            .unwrap(),
            b"final bytes"
        );
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn finalize_no_replace_fails_closed_without_linking_on_unsupported_platform() {
        let temp = tempdir().unwrap();
        let root = existing_root(&temp);
        let storage = Storage::new(root.clone()).unwrap();
        storage.initialize().await.unwrap();
        let project_id = Uuid::parse_str("018f1010-7b2a-7000-8000-000000000007").unwrap();
        let upload_id = Uuid::parse_str("018f1010-7b2a-7000-8000-000000000008").unwrap();
        let name = SafeFileName::parse("Report Final.pdf").unwrap();
        storage.create_project_dir(project_id).await.unwrap();
        storage.create_staging(upload_id).await.unwrap();
        storage
            .write_chunk(upload_id, 0, &b"final bytes"[..])
            .await
            .unwrap();

        let error = storage
            .finalize_no_replace(upload_id, project_id, &name)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            StorageError::Io { source } if source.kind() == io::ErrorKind::Unsupported
        ));
        assert_eq!(storage.staging_len(upload_id).await.unwrap(), Some(11));
        assert_eq!(
            storage.final_file_len(project_id, &name).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn finalize_conflict_preserves_destination_and_staging() {
        let temp = tempdir().unwrap();
        let root = existing_root(&temp);
        let storage = Storage::new(root.clone()).unwrap();
        storage.initialize().await.unwrap();
        let project_id = Uuid::parse_str("018f1010-7b2a-7000-8000-000000000009").unwrap();
        let upload_id = Uuid::parse_str("018f1010-7b2a-7000-8000-00000000000a").unwrap();
        let name = SafeFileName::parse("same.txt").unwrap();
        storage.create_project_dir(project_id).await.unwrap();
        let destination = root
            .join("projects")
            .join(project_id.to_string())
            .join("files")
            .join(name.as_str());
        std::fs::write(&destination, b"original").unwrap();
        storage.create_staging(upload_id).await.unwrap();
        storage
            .write_chunk(upload_id, 0, &b"replacement"[..])
            .await
            .unwrap();

        assert!(matches!(
            storage
                .finalize_no_replace(upload_id, project_id, &name)
                .await,
            Err(StorageError::AlreadyExists)
        ));
        assert_eq!(std::fs::read(destination).unwrap(), b"original");
        assert_eq!(storage.staging_len(upload_id).await.unwrap(), Some(11));
    }

    #[tokio::test]
    async fn finalize_classifies_unsafe_exact_leaves_without_changing_them() {
        let temp = tempdir().unwrap();
        let root = existing_root(&temp);
        let storage = Storage::new(root.clone()).unwrap();
        storage.initialize().await.unwrap();
        let project_id = Uuid::parse_str("018f1010-7b2a-7000-8000-000000000019").unwrap();
        storage.create_project_dir(project_id).await.unwrap();

        let unsafe_staging = Uuid::parse_str("018f1010-7b2a-7000-8000-00000000001a").unwrap();
        storage.create_staging(unsafe_staging).await.unwrap();
        let unsafe_staging_path = root
            .join(".cellar/uploads")
            .join(format!("{unsafe_staging}.part"));
        std::fs::remove_file(&unsafe_staging_path).unwrap();
        std::fs::create_dir(&unsafe_staging_path).unwrap();
        let staging_name = SafeFileName::parse("staging.txt").unwrap();
        assert!(matches!(
            storage
                .finalize_no_replace(unsafe_staging, project_id, &staging_name)
                .await,
            Err(StorageError::UnsafeEntry)
        ));
        assert!(std::fs::metadata(&unsafe_staging_path).unwrap().is_dir());

        let unsafe_destination = Uuid::parse_str("018f1010-7b2a-7000-8000-00000000001b").unwrap();
        storage.create_staging(unsafe_destination).await.unwrap();
        let destination_name = SafeFileName::parse("destination.txt").unwrap();
        let unsafe_destination_path = root
            .join("projects")
            .join(project_id.to_string())
            .join("files")
            .join(destination_name.as_str());
        std::fs::create_dir(&unsafe_destination_path).unwrap();
        assert!(matches!(
            storage
                .finalize_no_replace(unsafe_destination, project_id, &destination_name)
                .await,
            Err(StorageError::UnsafeEntry)
        ));
        assert!(
            std::fs::metadata(&unsafe_destination_path)
                .unwrap()
                .is_dir()
        );
        assert_eq!(
            storage.staging_len(unsafe_destination).await.unwrap(),
            Some(0)
        );
    }

    #[tokio::test]
    async fn final_length_and_listing_reflect_regular_disk_files_only() {
        let temp = tempdir().unwrap();
        let root = existing_root(&temp);
        let storage = Storage::new(root.clone()).unwrap();
        storage.initialize().await.unwrap();
        let project_id = Uuid::parse_str("018f1010-7b2a-7000-8000-00000000000b").unwrap();
        storage.create_project_dir(project_id).await.unwrap();
        let files_dir = root
            .join("projects")
            .join(project_id.to_string())
            .join("files");
        let alpha = SafeFileName::parse("Alpha.txt").unwrap();

        assert_eq!(
            storage.final_file_len(project_id, &alpha).await.unwrap(),
            None
        );
        std::fs::write(files_dir.join(alpha.as_str()), b"alpha").unwrap();
        std::fs::write(files_dir.join("Beta.bin"), b"1234567").unwrap();
        std::fs::create_dir(files_dir.join("nested")).unwrap();
        let target = root.join("target.txt");
        std::fs::write(&target, b"linked").unwrap();
        let symlink_created = try_symlink_file(&target, &files_dir.join("linked.txt"));

        assert!(
            storage
                .destination_exists(project_id, &alpha)
                .await
                .unwrap()
        );
        assert_eq!(
            storage.final_file_len(project_id, &alpha).await.unwrap(),
            Some(5)
        );
        let mut opened = storage.open_final_file(project_id, &alpha).await.unwrap();
        let mut bytes = Vec::new();
        opened.read_to_end(&mut bytes).await.unwrap();
        assert_eq!(bytes, b"alpha");
        let mut listed = storage.list_files(project_id).await.unwrap();
        listed.sort_by(|left, right| left.name().cmp(right.name()));
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].name().as_str(), "Alpha.txt");
        assert_eq!(listed[0].size(), 5);
        assert!(listed[0].modified_at() <= SystemTime::now());
        assert_eq!(listed[1].name().as_str(), "Beta.bin");
        assert_eq!(listed[1].size(), 7);
        if symlink_created {
            let linked = SafeFileName::parse("linked.txt").unwrap();
            assert!(matches!(
                storage.open_final_file(project_id, &linked).await,
                Err(StorageError::UnsafeEntry)
            ));
            assert_eq!(std::fs::read(target).unwrap(), b"linked");
        }
    }

    #[tokio::test]
    async fn listing_ignores_regular_disk_file_with_invalid_safe_name() {
        let temp = tempdir().unwrap();
        let root = existing_root(&temp);
        let storage = Storage::new(root.clone()).unwrap();
        storage.initialize().await.unwrap();
        let project_id = Uuid::parse_str("018f1010-7b2a-7000-8000-00000000000c").unwrap();
        storage.create_project_dir(project_id).await.unwrap();
        let invalid = root
            .join("projects")
            .join(project_id.to_string())
            .join("files")
            .join("invalid\u{7f}.txt");
        std::fs::write(invalid, b"unsafe").unwrap();

        assert!(storage.list_files(project_id).await.unwrap().is_empty());
    }

    #[test]
    fn storage_debug_and_errors_do_not_reveal_absolute_root() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("private-data");
        std::fs::create_dir(&root).unwrap();
        let root_text = root.display().to_string();
        let storage = Storage::new(root).unwrap();
        let error = StorageError::Io {
            source: io::Error::other(root_text.clone()),
        };

        assert!(!format!("{storage:?}").contains(&root_text));
        assert!(!format!("{error}").contains(&root_text));
        assert!(!format!("{error:?}").contains(&root_text));
        assert!(error.source().unwrap().to_string().contains(&root_text));
    }

    #[test]
    fn storage_full_error_kind_maps_to_insufficient_space() {
        assert!(matches!(
            map_io(io::Error::from(io::ErrorKind::StorageFull)),
            StorageError::InsufficientSpace
        ));
    }

    fn existing_root(temp: &TempDir) -> PathBuf {
        let root = temp.path().join("data");
        std::fs::create_dir(&root).unwrap();
        root
    }

    #[cfg(windows)]
    fn root_alias(root: &Path) -> PathBuf {
        PathBuf::from(format!(r"\\?\{}", root.display()))
    }

    #[cfg(not(windows))]
    fn root_alias(root: &Path) -> PathBuf {
        root.join(".")
    }

    #[cfg(windows)]
    fn try_symlink_file(target: &Path, link: &Path) -> bool {
        classify_windows_symlink_result(std::os::windows::fs::symlink_file(target, link))
    }

    #[cfg(windows)]
    fn try_symlink_dir(target: &Path, link: &Path) -> bool {
        classify_windows_symlink_result(std::os::windows::fs::symlink_dir(target, link))
    }

    #[cfg(windows)]
    fn classify_windows_symlink_result(result: io::Result<()>) -> bool {
        match result {
            Ok(()) => true,
            Err(error) if matches!(error.raw_os_error(), Some(1 | 50 | 1314)) => {
                eprintln!(
                    "skipping symlink assertions: Windows privilege or symlink support unavailable ({:?})",
                    error.raw_os_error()
                );
                false
            }
            Err(error) => panic!("unexpected test symlink creation failure: {error}"),
        }
    }

    #[cfg(unix)]
    fn try_symlink_file(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link)
            .unwrap_or_else(|error| panic!("unexpected test symlink creation failure: {error}"));
        true
    }

    #[cfg(unix)]
    fn try_symlink_dir(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link)
            .unwrap_or_else(|error| panic!("unexpected test symlink creation failure: {error}"));
        true
    }
}
