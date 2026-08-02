use async_trait::async_trait;
use thiserror::Error;

use crate::SafeName;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    File,
    Directory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileIdentity {
    pub volume_serial: u64,
    pub file_id: u128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageErrorKind {
    InvalidName,
    NotFound,
    Conflict,
    Unsupported,
    AccessDenied,
    CleanupFailed,
    Io,
}

#[derive(Debug, Error)]
#[error("storage operation failed: {kind:?}")]
pub struct StorageError {
    kind: StorageErrorKind,
}

impl StorageError {
    pub const fn new(kind: StorageErrorKind) -> Self {
        Self { kind }
    }

    pub const fn kind(&self) -> StorageErrorKind {
        self.kind
    }

    pub const fn code(&self) -> &'static str {
        match self.kind {
            StorageErrorKind::InvalidName => "invalid_name",
            StorageErrorKind::NotFound => "not_found",
            StorageErrorKind::Conflict => "conflict",
            StorageErrorKind::Unsupported => "unsupported",
            StorageErrorKind::AccessDenied => "access_denied",
            StorageErrorKind::CleanupFailed => "cleanup_failed",
            StorageErrorKind::Io => "io_error",
        }
    }
}

/// Platform-neutral handle-relative storage boundary.
#[async_trait]
pub trait Storage: Send + Sync {
    type Handle: Clone + Send + Sync;

    async fn open_verified(
        &self,
        parent: &Self::Handle,
        name: &SafeName,
    ) -> Result<Self::Handle, StorageError>;

    async fn rename_no_replace(
        &self,
        source: &Self::Handle,
        destination_parent: &Self::Handle,
        destination_name: &SafeName,
    ) -> Result<FileIdentity, StorageError>;
}
