use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use cellar_core::{
    PublicationPresence, PublishedUpload, StagingIdentity, UploadCommitIntent, UploadId,
    UploadPublicationError, UploadPublicationObservation, UploadPublisher, UploadStagingError,
    UploadStagingStore, VerifiedUpload,
};
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

    fn staging_handle(&self, id: UploadId) -> Result<VerifiedHandle, UploadPublicationError> {
        let name = Self::name(id).map_err(|_| UploadPublicationError::Unavailable)?;
        self.storage
            .open_verified(&self.directory, &name)
            .map_err(map_publication_storage)
    }

    fn destination_parent(
        &self,
        intent: &UploadCommitIntent,
    ) -> Result<VerifiedHandle, UploadPublicationError> {
        let projects =
            WindowsName::parse("projects").map_err(|_| UploadPublicationError::Unavailable)?;
        let mut current = self
            .storage
            .open_verified(self.storage.root(), &projects)
            .map_err(map_publication_storage)?;
        for component in [intent.project_id.to_string(), "files".to_owned()]
            .into_iter()
            .chain(intent.destination_components.iter().cloned())
        {
            let name =
                WindowsName::parse(component).map_err(|_| UploadPublicationError::Conflict)?;
            current = self
                .storage
                .open_verified(&current, &name)
                .map_err(map_publication_storage)?;
            if current.kind() != EntryKind::Directory {
                return Err(UploadPublicationError::Conflict);
            }
        }
        if let Some(expected) = intent.destination_parent_identity
            && staging_identity(current.identity()) != expected
        {
            return Err(UploadPublicationError::Conflict);
        }
        Ok(current)
    }

    fn destination_handle(
        &self,
        intent: &UploadCommitIntent,
    ) -> Result<VerifiedHandle, UploadPublicationError> {
        let parent = self.destination_parent(intent)?;
        let name = WindowsName::parse(intent.destination_name.clone())
            .map_err(|_| UploadPublicationError::Conflict)?;
        self.storage
            .open_verified(&parent, &name)
            .map_err(map_publication_storage)
    }

    fn presence(
        result: Result<VerifiedHandle, UploadPublicationError>,
        expected: StagingIdentity,
    ) -> Result<PublicationPresence, UploadPublicationError> {
        match result {
            Ok(handle) => {
                let actual = staging_identity(handle.identity());
                Ok(if handle.kind() == EntryKind::File && actual == expected {
                    PublicationPresence::Expected
                } else {
                    PublicationPresence::Unexpected
                })
            }
            Err(UploadPublicationError::NotFound) => Ok(PublicationPresence::Absent),
            Err(error) => Err(error),
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

#[async_trait]
impl UploadPublisher for WindowsUploadStaging {
    async fn verify_and_close(
        &self,
        id: UploadId,
        expected_size: i64,
    ) -> Result<VerifiedUpload, UploadPublicationError> {
        let this = self.clone();
        let entry = self.entry(id);
        tokio::task::spawn_blocking(move || {
            let _guard = entry.io.blocking_lock();
            let result = (|| {
                let handle = this
                    .ensure_handle(id, &entry)
                    .map_err(map_staging_publication)?;
                this.storage
                    .flush_file(&handle)
                    .map_err(map_publication_storage)?;
                let identity = staging_identity(handle.identity());
                let length = this
                    .storage
                    .file_length(&handle)
                    .map_err(map_publication_storage)?;
                if length != expected_size {
                    return Err(UploadPublicationError::Conflict);
                }
                let mut hasher = Sha256::new();
                let mut offset = 0_i64;
                const VERIFY_BLOCK: i64 = 8 * 1024 * 1024;
                while offset < length {
                    let block = (length - offset).min(VERIFY_BLOCK);
                    let bytes = this
                        .storage
                        .read_exact_at(&handle, offset, block)
                        .map_err(map_publication_storage)?;
                    hasher.update(bytes);
                    offset += block;
                }
                Ok(VerifiedUpload {
                    size: length,
                    sha256: hasher.finalize().into(),
                    staging_identity: identity,
                })
            })();
            this.release_entry(id, &entry);
            result
        })
        .await
        .map_err(|_| UploadPublicationError::Unavailable)?
    }

    async fn observe(
        &self,
        intent: &UploadCommitIntent,
    ) -> Result<UploadPublicationObservation, UploadPublicationError> {
        let this = self.clone();
        let intent = intent.clone();
        tokio::task::spawn_blocking(move || {
            Ok(UploadPublicationObservation {
                staging: Self::presence(
                    this.staging_handle(intent.upload_id),
                    intent.staging_identity,
                )?,
                destination: Self::presence(
                    this.destination_handle(&intent),
                    intent.staging_identity,
                )?,
            })
        })
        .await
        .map_err(|_| UploadPublicationError::Unavailable)?
    }

    async fn publish_no_replace(
        &self,
        intent: &UploadCommitIntent,
    ) -> Result<PublishedUpload, UploadPublicationError> {
        let this = self.clone();
        let intent = intent.clone();
        tokio::task::spawn_blocking(move || {
            let source = this.staging_handle(intent.upload_id)?;
            if source.kind() != EntryKind::File
                || staging_identity(source.identity()) != intent.staging_identity
            {
                return Err(UploadPublicationError::Conflict);
            }
            let parent = this.destination_parent(&intent)?;
            let name = WindowsName::parse(intent.destination_name.clone())
                .map_err(|_| UploadPublicationError::Conflict)?;
            let renamed = this
                .storage
                .rename_no_replace(&source, &parent, &name)
                .map_err(map_publication_storage)?;
            if staging_identity(renamed) != intent.staging_identity {
                return Err(UploadPublicationError::Conflict);
            }
            // The retained rename handle deliberately does not share DELETE.
            // Close it before reopening the destination for independent
            // post-rename identity verification.
            drop(source);
            let destination = this
                .storage
                .open_verified(&parent, &name)
                .map_err(map_publication_storage)?;
            published_facts(&this.storage, &destination, intent.staging_identity)
        })
        .await
        .map_err(|_| UploadPublicationError::Unavailable)?
    }

    async fn inspect_destination(
        &self,
        intent: &UploadCommitIntent,
    ) -> Result<PublishedUpload, UploadPublicationError> {
        let this = self.clone();
        let intent = intent.clone();
        tokio::task::spawn_blocking(move || {
            let destination = this.destination_handle(&intent)?;
            published_facts(&this.storage, &destination, intent.staging_identity)
        })
        .await
        .map_err(|_| UploadPublicationError::Unavailable)?
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

fn map_staging_publication(error: UploadStagingError) -> UploadPublicationError {
    match error {
        UploadStagingError::NotFound => UploadPublicationError::NotFound,
        UploadStagingError::InsufficientStorage => UploadPublicationError::InsufficientStorage,
        UploadStagingError::Unavailable => UploadPublicationError::Unavailable,
    }
}

fn map_publication_storage(error: StorageError) -> UploadPublicationError {
    match error.kind() {
        StorageErrorKind::NotFound => UploadPublicationError::NotFound,
        StorageErrorKind::Conflict => UploadPublicationError::Conflict,
        StorageErrorKind::InsufficientStorage => UploadPublicationError::InsufficientStorage,
        _ => UploadPublicationError::Unavailable,
    }
}

fn staging_identity(identity: cellar_storage::FileIdentity) -> StagingIdentity {
    let mut bytes = [0_u8; 24];
    bytes[..8].copy_from_slice(&identity.volume_serial.to_le_bytes());
    bytes[8..].copy_from_slice(&identity.file_id.to_le_bytes());
    StagingIdentity::new(bytes)
}

fn published_facts(
    storage: &WindowsStorage,
    handle: &VerifiedHandle,
    expected: StagingIdentity,
) -> Result<PublishedUpload, UploadPublicationError> {
    if handle.kind() != EntryKind::File || staging_identity(handle.identity()) != expected {
        return Err(UploadPublicationError::Conflict);
    }
    let (size, mtime_filetime_100ns) = storage
        .file_length_and_mtime(handle)
        .map_err(map_publication_storage)?;
    Ok(PublishedUpload {
        identity: expected,
        size,
        mtime_filetime_100ns,
    })
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
