use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{FileEntryId, ProjectId};

pub const DEFAULT_FILE_LIST_LIMIT: u32 = 100;
pub const MAX_FILE_LIST_LIMIT: u32 = 500;
pub const MAX_FILE_EXACT_NAME_BYTES: usize = 4 * 255;
pub const MAX_FILE_EXACT_NAME_UTF16_UNITS: usize = 255;
pub const MAX_FILE_PLATFORM_KIND_BYTES: usize = 64;

#[derive(Clone, Eq, PartialEq)]
pub struct FileExactName(String);

impl FileExactName {
    pub fn parse(value: impl Into<String>) -> Result<Self, FileValidationError> {
        let value = value.into();
        if value.is_empty()
            || value == "."
            || value == ".."
            || value.len() > MAX_FILE_EXACT_NAME_BYTES
            || value.encode_utf16().count() > MAX_FILE_EXACT_NAME_UTF16_UNITS
            || value.contains(['\0', '/', '\\'])
            || value.chars().any(char::is_control)
        {
            return Err(FileValidationError::InvalidExactName);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for FileExactName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("FileExactName(<redacted>)")
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    File,
    Directory,
}

impl FromStr for FileKind {
    type Err = FileValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "file" => Ok(Self::File),
            "directory" => Ok(Self::Directory),
            _ => Err(FileValidationError::InvalidKind),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FileState {
    Live,
    Settling,
    Missing,
    Trashed,
    Unsupported,
}

impl FileState {
    #[must_use]
    pub const fn is_listable(self) -> bool {
        matches!(self, Self::Live | Self::Settling | Self::Unsupported)
    }
}

impl FromStr for FileState {
    type Err = FileValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "live" => Ok(Self::Live),
            "settling" => Ok(Self::Settling),
            "missing" => Ok(Self::Missing),
            "trashed" => Ok(Self::Trashed),
            "unsupported" => Ok(Self::Unsupported),
            _ => Err(FileValidationError::InvalidState),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FileHashState {
    Unknown,
    Queued,
    Computing,
    Ready,
    Failed,
}

impl FromStr for FileHashState {
    type Err = FileValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "unknown" => Ok(Self::Unknown),
            "queued" => Ok(Self::Queued),
            "computing" => Ok(Self::Computing),
            "ready" => Ok(Self::Ready),
            "failed" => Ok(Self::Failed),
            _ => Err(FileValidationError::InvalidHashState),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct PlatformIdentity {
    kind: String,
    volume_serial: Option<[u8; 8]>,
    filesystem_file_id: Option<[u8; 16]>,
}

impl PlatformIdentity {
    pub fn try_new(
        kind: impl Into<String>,
        volume_serial: Option<Vec<u8>>,
        filesystem_file_id: Option<Vec<u8>>,
    ) -> Result<Self, FileValidationError> {
        let kind = kind.into();
        if kind.is_empty()
            || kind.len() > MAX_FILE_PLATFORM_KIND_BYTES
            || !kind
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(FileValidationError::InvalidPlatformIdentity);
        }
        let volume_serial = volume_serial
            .map(|value| {
                value
                    .try_into()
                    .map_err(|_| FileValidationError::InvalidPlatformIdentity)
            })
            .transpose()?;
        let filesystem_file_id = filesystem_file_id
            .map(|value| {
                value
                    .try_into()
                    .map_err(|_| FileValidationError::InvalidPlatformIdentity)
            })
            .transpose()?;
        if volume_serial.is_some() != filesystem_file_id.is_some() {
            return Err(FileValidationError::InvalidPlatformIdentity);
        }
        Ok(Self {
            kind,
            volume_serial,
            filesystem_file_id,
        })
    }

    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    #[must_use]
    pub fn opaque_bytes(&self) -> Option<[u8; 24]> {
        let mut bytes = [0_u8; 24];
        bytes[..8].copy_from_slice(self.volume_serial.as_ref()?);
        bytes[8..].copy_from_slice(self.filesystem_file_id.as_ref()?);
        Some(bytes)
    }
}

impl fmt::Debug for PlatformIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlatformIdentity")
            .field("kind", &self.kind)
            .field("opaque", &self.opaque_bytes().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileEntry {
    pub id: FileEntryId,
    pub project_id: ProjectId,
    pub parent_id: Option<FileEntryId>,
    pub exact_name: FileExactName,
    pub relative_path: String,
    pub kind: FileKind,
    pub platform_identity: PlatformIdentity,
    pub size: i64,
    pub mtime_filetime_100ns: i64,
    pub hash: Option<[u8; 32]>,
    pub hash_state: FileHashState,
    pub state: FileState,
    pub revision: i64,
    pub scan_generation: i64,
    pub observed_at: OffsetDateTime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileCursor {
    project_id: ProjectId,
    parent_id: Option<FileEntryId>,
    snapshot_version: i64,
    exact_name: FileExactName,
    entry_id: FileEntryId,
}

impl FileCursor {
    pub fn try_new(
        project_id: ProjectId,
        parent_id: Option<FileEntryId>,
        snapshot_version: i64,
        exact_name: impl Into<String>,
        entry_id: FileEntryId,
    ) -> Result<Self, FileValidationError> {
        if snapshot_version < 0 {
            return Err(FileValidationError::InvalidSnapshot);
        }
        Ok(Self {
            project_id,
            parent_id,
            snapshot_version,
            exact_name: FileExactName::parse(exact_name)?,
            entry_id,
        })
    }

    #[must_use]
    pub const fn project_id(&self) -> ProjectId {
        self.project_id
    }

    #[must_use]
    pub const fn parent_id(&self) -> Option<FileEntryId> {
        self.parent_id
    }

    #[must_use]
    pub const fn snapshot_version(&self) -> i64 {
        self.snapshot_version
    }

    #[must_use]
    pub fn exact_name(&self) -> &str {
        self.exact_name.as_str()
    }

    #[must_use]
    pub const fn entry_id(&self) -> FileEntryId {
        self.entry_id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileListRequest {
    project_id: ProjectId,
    parent_id: Option<FileEntryId>,
    cursor: Option<FileCursor>,
    limit: u32,
}

impl FileListRequest {
    pub fn first(
        project_id: ProjectId,
        parent_id: Option<FileEntryId>,
        limit: u32,
    ) -> Result<Self, FileValidationError> {
        validate_limit(limit)?;
        Ok(Self {
            project_id,
            parent_id,
            cursor: None,
            limit,
        })
    }

    pub fn after(cursor: FileCursor, limit: u32) -> Result<Self, FileValidationError> {
        validate_limit(limit)?;
        Ok(Self {
            project_id: cursor.project_id,
            parent_id: cursor.parent_id,
            cursor: Some(cursor),
            limit,
        })
    }

    #[must_use]
    pub const fn project_id(&self) -> ProjectId {
        self.project_id
    }

    #[must_use]
    pub const fn parent_id(&self) -> Option<FileEntryId> {
        self.parent_id
    }

    #[must_use]
    pub fn cursor(&self) -> Option<&FileCursor> {
        self.cursor.as_ref()
    }

    #[must_use]
    pub const fn limit(&self) -> u32 {
        self.limit
    }
}

fn validate_limit(limit: u32) -> Result<(), FileValidationError> {
    if limit == 0 || limit > MAX_FILE_LIST_LIMIT {
        Err(FileValidationError::InvalidLimit)
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilePage {
    pub items: Vec<FileEntry>,
    pub next_cursor: Option<FileCursor>,
    pub snapshot_version: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileValidationError {
    InvalidExactName,
    InvalidKind,
    InvalidState,
    InvalidHashState,
    InvalidPlatformIdentity,
    InvalidSnapshot,
    InvalidLimit,
}

impl fmt::Display for FileValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidExactName => "invalid_file_name",
            Self::InvalidKind => "invalid_file_kind",
            Self::InvalidState => "invalid_file_state",
            Self::InvalidHashState => "invalid_file_hash_state",
            Self::InvalidPlatformIdentity => "invalid_platform_identity",
            Self::InvalidSnapshot => "invalid_file_snapshot",
            Self::InvalidLimit => "invalid_file_limit",
        })
    }
}

impl std::error::Error for FileValidationError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileRepositoryError {
    ProjectNotFound,
    FolderNotFound,
    InvalidFolder,
    UnsupportedFolder,
    SnapshotChanged,
    Unavailable,
}

impl FileRepositoryError {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::ProjectNotFound => "project_not_found",
            Self::FolderNotFound => "folder_not_found",
            Self::InvalidFolder => "not_a_folder",
            Self::UnsupportedFolder => "unsupported_file_entry",
            Self::SnapshotChanged => "stale_file_snapshot",
            Self::Unavailable => "file_catalog_unavailable",
        }
    }
}

impl fmt::Display for FileRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for FileRepositoryError {}

#[async_trait]
pub trait FileRepository: Send + Sync {
    /// Returns one catalog page from a single database read snapshot.
    ///
    /// Every durable `file_entry` mutation must increment the affected
    /// project's epoch in the same transaction. A cursor whose epoch differs
    /// from the current epoch is rejected instead of mixing snapshots.
    async fn list(&self, request: FileListRequest) -> Result<FilePage, FileRepositoryError>;
}

#[derive(Clone)]
pub struct FileService {
    repository: Arc<dyn FileRepository>,
}

impl FileService {
    #[must_use]
    pub fn new(repository: Arc<dyn FileRepository>) -> Self {
        Self { repository }
    }

    pub async fn list(&self, request: FileListRequest) -> Result<FilePage, FileRepositoryError> {
        self.repository.list(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_states_and_limits_are_bounded() {
        assert!(FileExactName::parse("report.txt").is_ok());
        for invalid in ["", ".", "..", "a/b", "a\\b", "nul\0name"] {
            assert!(FileExactName::parse(invalid).is_err());
        }
        assert!(FileExactName::parse("x".repeat(MAX_FILE_EXACT_NAME_UTF16_UNITS + 1)).is_err());
        assert!(FileState::Live.is_listable());
        assert!(FileState::Settling.is_listable());
        assert!(FileState::Unsupported.is_listable());
        assert!(!FileState::Missing.is_listable());
        assert!(!FileState::Trashed.is_listable());
        assert!(FileListRequest::first(ProjectId::new(), None, 0).is_err());
        assert!(FileListRequest::first(ProjectId::new(), None, MAX_FILE_LIST_LIMIT + 1).is_err());
    }

    #[test]
    fn platform_identity_is_opaque_and_strictly_sized() {
        let identity =
            PlatformIdentity::try_new("windows_file_id", Some(vec![1; 8]), Some(vec![2; 16]))
                .unwrap();
        assert_eq!(identity.kind(), "windows_file_id");
        assert_eq!(identity.opaque_bytes().unwrap().len(), 24);
        assert!(!format!("{identity:?}").contains("[1"));
        assert!(PlatformIdentity::try_new("x", Some(vec![0; 7]), Some(vec![0; 16])).is_err());
        assert!(PlatformIdentity::try_new("x", None, Some(vec![0; 16])).is_err());
    }
}
