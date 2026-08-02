use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use cellar_core::{StagingIdentity, UploadId, UploadStagingError, UploadStagingStore};
use cellar_storage::{EntryKind, StorageError, StorageErrorKind};
use sha2::{Digest as _, Sha256};
use tokio::sync::Mutex;

use crate::{VerifiedHandle, WindowsName, WindowsStorage};

const STAGING_DIRECTORY: &str = ".cellar-upload-staging";

struct StagingEntry {
    io: Arc<Mutex<()>>,
    handle: StdMutex<Option<VerifiedHandle>>,
}

impl StagingEntry {
    fn empty() -> Self {
        Self {
            io: Arc::new(Mutex::new(())),
            handle: StdMutex::new(None),
        }
    }
}

#[derive(Clone)]
pub struct WindowsUploadStaging {
    storage: WindowsStorage,
    directory: VerifiedHandle,
    entries: Arc<StdMutex<HashMap<UploadId, Arc<StagingEntry>>>>,
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
        Ok(Self {
            storage,
            directory,
            entries: Arc::new(StdMutex::new(HashMap::new())),
        })
    }

    fn name(id: UploadId) -> Result<WindowsName, UploadStagingError> {
        WindowsName::parse(format!("{id}.part")).map_err(|_| UploadStagingError::Unavailable)
    }

    fn entry(&self, id: UploadId) -> Arc<StagingEntry> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(id)
            .or_insert_with(|| Arc::new(StagingEntry::empty()))
            .clone()
    }

    fn ensure_handle(
        &self,
        id: UploadId,
        entry: &Arc<StagingEntry>,
    ) -> Result<VerifiedHandle, UploadStagingError> {
        let mut slot = entry
            .handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(handle) = slot.as_ref() {
            return Ok(handle.clone());
        }
        let name = Self::name(id)?;
        let handle = match self.storage.open_verified_writable(&self.directory, &name) {
            Ok(handle) => handle,
            Err(error) => {
                drop(slot);
                self.release_entry(id, entry);
                return Err(map_storage(error));
            }
        };
        *slot = Some(handle.clone());
        Ok(handle)
    }

    fn release_entry(&self, id: UploadId, entry: &Arc<StagingEntry>) {
        *entry
            .handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if entries
            .get(&id)
            .is_some_and(|current| Arc::ptr_eq(current, entry))
        {
            entries.remove(&id);
        }
    }
}

#[async_trait]
impl UploadStagingStore for WindowsUploadStaging {
    async fn create(&self, id: UploadId) -> Result<(), UploadStagingError> {
        let this = self.clone();
        let entry = self.entry(id);
        blocking(move || {
            let _guard = entry.io.blocking_lock();
            let name = Self::name(id)?;
            let handle = match this
                .storage
                .create_staging_file_no_replace(&this.directory, &name)
            {
                Ok(handle) => handle,
                Err(error) => {
                    this.release_entry(id, &entry);
                    return Err(map_storage(error));
                }
            };
            *entry
                .handle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(handle);
            Ok(())
        })
        .await
    }

    async fn length(&self, id: UploadId) -> Result<i64, UploadStagingError> {
        let this = self.clone();
        let entry = self.entry(id);
        blocking(move || {
            let _guard = entry.io.blocking_lock();
            let handle = this.ensure_handle(id, &entry)?;
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
        let entry = self.entry(id);
        blocking(move || {
            let _guard = entry.io.blocking_lock();
            let handle = this.ensure_handle(id, &entry)?;
            this.storage
                .read_exact_at(&handle, offset, length)
                .map_err(map_storage)
        })
        .await
    }

    async fn truncate(&self, id: UploadId, length: i64) -> Result<(), UploadStagingError> {
        let this = self.clone();
        let entry = self.entry(id);
        blocking(move || {
            let _guard = entry.io.blocking_lock();
            let handle = this.ensure_handle(id, &entry)?;
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
        let entry = self.entry(id);
        let bytes = bytes.to_vec();
        blocking(move || {
            let _guard = entry.io.blocking_lock();
            let handle = this.ensure_handle(id, &entry)?;
            this.storage
                .write_exact_at_and_flush(&handle, offset, &bytes)
                .map_err(map_storage)
        })
        .await
    }

    async fn write_verify_and_flush(
        &self,
        id: UploadId,
        offset: i64,
        bytes: &[u8],
        digest: [u8; 32],
    ) -> Result<i64, UploadStagingError> {
        let this = self.clone();
        let entry = self.entry(id);
        let bytes = bytes.to_vec();
        blocking(move || {
            let _guard = entry.io.blocking_lock();
            let handle = this.ensure_handle(id, &entry)?;
            this.storage
                .write_exact_at_and_flush(&handle, offset, &bytes)
                .map_err(map_storage)?;
            let length = this.storage.file_length(&handle).map_err(map_storage)?;
            let durable = this
                .storage
                .read_exact_at(
                    &handle,
                    offset,
                    i64::try_from(bytes.len()).map_err(|_| UploadStagingError::Unavailable)?,
                )
                .map_err(map_storage)?;
            if <[u8; 32]>::from(Sha256::digest(&durable)) != digest {
                return Err(UploadStagingError::Unavailable);
            }
            Ok(length)
        })
        .await
    }

    async fn remove(&self, id: UploadId) -> Result<(), UploadStagingError> {
        let this = self.clone();
        let entry = self.entry(id);
        blocking(move || {
            let _guard = entry.io.blocking_lock();
            let handle = this.ensure_handle(id, &entry)?;
            this.storage.remove_file(handle).map_err(map_storage)?;
            this.release_entry(id, &entry);
            Ok(())
        })
        .await
    }

    async fn available_space(&self) -> Result<i64, UploadStagingError> {
        let storage = self.storage.clone();
        blocking(move || storage.available_space().map_err(map_storage)).await
    }

    async fn identity(&self, id: UploadId) -> Result<Option<StagingIdentity>, UploadStagingError> {
        let this = self.clone();
        let entry = self.entry(id);
        blocking(move || {
            let _guard = entry.io.blocking_lock();
            let handle = this.ensure_handle(id, &entry)?;
            let identity = handle.identity();
            let mut bytes = [0_u8; 24];
            bytes[..8].copy_from_slice(&identity.volume_serial.to_le_bytes());
            bytes[8..].copy_from_slice(&identity.file_id.to_le_bytes());
            Ok(Some(StagingIdentity::new(bytes)))
        })
        .await
    }

    async fn release(&self, id: UploadId) {
        let entry = self.entry(id);
        let this = self.clone();
        let _ = blocking(move || {
            let _guard = entry.io.blocking_lock();
            this.release_entry(id, &entry);
            Ok(())
        })
        .await;
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

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn cancelled_future_keeps_per_upload_io_owned_until_blocking_worker_exits() {
        let directory = tempdir().unwrap();
        let identity = crate::preflight::open_as_service(directory.path()).unwrap();
        let storage = WindowsStorage::adopt(identity).unwrap();
        let staging = WindowsUploadStaging::open(storage).unwrap();
        let id = UploadId::new();
        staging.create(id).await.unwrap();
        let entry = staging.entry(id);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = tokio::spawn(blocking(move || {
            let _guard = entry.io.blocking_lock();
            let _ = started_tx.send(());
            release_rx.recv().unwrap();
            Ok(())
        }));
        started_rx.await.unwrap();
        worker.abort();

        let waiting = tokio::spawn({
            let staging = staging.clone();
            async move { staging.length(id).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        assert!(
            !waiting.is_finished(),
            "detached worker released per-upload ownership"
        );
        release_tx.send(()).unwrap();
        assert_eq!(waiting.await.unwrap().unwrap(), 0);
    }

    #[tokio::test]
    async fn failed_open_releases_new_adapter_entry_and_retry_can_create() {
        let directory = tempdir().unwrap();
        let identity = crate::preflight::open_as_service(directory.path()).unwrap();
        let storage = WindowsStorage::adopt(identity).unwrap();
        let staging = WindowsUploadStaging::open(storage).unwrap();
        let id = UploadId::new();

        assert_eq!(
            staging.length(id).await.unwrap_err(),
            UploadStagingError::NotFound
        );
        assert!(!staging.entries.lock().unwrap().contains_key(&id));
        staging.create(id).await.unwrap();
        assert_eq!(staging.length(id).await.unwrap(), 0);
    }
}
