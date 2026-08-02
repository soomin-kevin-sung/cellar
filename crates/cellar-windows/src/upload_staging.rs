use async_trait::async_trait;
use cellar_core::{UploadId, UploadStagingError, UploadStagingStore};
use cellar_storage::{EntryKind, StorageError, StorageErrorKind};

use crate::{VerifiedHandle, WindowsName, WindowsStorage};

const STAGING_DIRECTORY: &str = ".cellar-upload-staging";

#[derive(Clone)]
pub struct WindowsUploadStaging {
    storage: WindowsStorage,
    directory: VerifiedHandle,
}

impl WindowsUploadStaging {
    pub fn open(storage: WindowsStorage) -> Result<Self, StorageError> {
        let name = WindowsName::parse(STAGING_DIRECTORY)
            .map_err(|_| StorageError::new(StorageErrorKind::InvalidName))?;
        let directory = match storage.create_directory_no_replace(storage.root(), &name) {
            Ok(handle) => handle,
            Err(error) if error.kind() == StorageErrorKind::Conflict => {
                storage.open_verified(storage.root(), &name)?
            }
            Err(error) => return Err(error),
        };
        if directory.kind() != EntryKind::Directory {
            return Err(StorageError::new(StorageErrorKind::Unsupported));
        }
        Ok(Self { storage, directory })
    }

    fn name(id: UploadId) -> Result<WindowsName, UploadStagingError> {
        WindowsName::parse(format!("{id}.part")).map_err(|_| UploadStagingError::Unavailable)
    }

    fn open_file(&self, id: UploadId) -> Result<VerifiedHandle, UploadStagingError> {
        let name = Self::name(id)?;
        self.storage
            .open_verified_writable(&self.directory, &name)
            .map_err(map_storage)
    }
}

#[async_trait]
impl UploadStagingStore for WindowsUploadStaging {
    async fn create(&self, id: UploadId) -> Result<(), UploadStagingError> {
        let this = self.clone();
        blocking(move || {
            let name = Self::name(id)?;
            this.storage
                .create_file_no_replace(&this.directory, &name)
                .map(|_| ())
                .map_err(map_storage)
        })
        .await
    }

    async fn length(&self, id: UploadId) -> Result<i64, UploadStagingError> {
        let this = self.clone();
        blocking(move || {
            let handle = this.open_file(id)?;
            this.storage.file_length(&handle).map_err(map_storage)
        })
        .await
    }

    async fn read_exact(
        &self,
        id: UploadId,
        offset: i64,
        length: i64,
    ) -> Result<Vec<u8>, UploadStagingError> {
        let this = self.clone();
        blocking(move || {
            let handle = this.open_file(id)?;
            this.storage
                .read_exact_at(&handle, offset, length)
                .map_err(map_storage)
        })
        .await
    }

    async fn truncate(&self, id: UploadId, length: i64) -> Result<(), UploadStagingError> {
        let this = self.clone();
        blocking(move || {
            let handle = this.open_file(id)?;
            this.storage
                .truncate_file(&handle, length)
                .map_err(map_storage)
        })
        .await
    }

    async fn write_exact_and_flush(
        &self,
        id: UploadId,
        offset: i64,
        bytes: &[u8],
    ) -> Result<(), UploadStagingError> {
        let this = self.clone();
        let bytes = bytes.to_vec();
        blocking(move || {
            let handle = this.open_file(id)?;
            this.storage
                .write_exact_at_and_flush(&handle, offset, &bytes)
                .map_err(map_storage)
        })
        .await
    }

    async fn remove(&self, id: UploadId) -> Result<(), UploadStagingError> {
        let this = self.clone();
        blocking(move || {
            let handle = this.open_file(id)?;
            this.storage.remove_file(handle).map_err(map_storage)
        })
        .await
    }

    async fn available_space(&self) -> Result<i64, UploadStagingError> {
        let storage = self.storage.clone();
        blocking(move || storage.available_space().map_err(map_storage)).await
    }
}

async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, UploadStagingError> + Send + 'static,
) -> Result<T, UploadStagingError> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| UploadStagingError::Unavailable)?
}

fn map_storage(error: StorageError) -> UploadStagingError {
    match error.kind() {
        StorageErrorKind::NotFound => UploadStagingError::NotFound,
        StorageErrorKind::InsufficientStorage => UploadStagingError::InsufficientStorage,
        _ => UploadStagingError::Unavailable,
    }
}
