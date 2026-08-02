use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use cellar_api::routes::files::{
    DownloadError, DownloadMetadata, DownloadReadError, DownloadSource, DownloadSpan,
    VerifiedDownload,
};
use cellar_core::{FileEntryId, ProjectId};
use sqlx::{Row, SqlitePool};
use tokio::sync::mpsc;

const MAX_DIRECTORY_DEPTH: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlatformFacts {
    pub volume_serial: u64,
    pub file_id: u128,
    pub length: u64,
    pub mtime_filetime_100ns: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlatformError {
    NotFound,
    Unsupported,
    Busy,
    Io,
}

impl fmt::Display for PlatformError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NotFound => "download_platform_not_found",
            Self::Unsupported => "download_platform_unsupported",
            Self::Busy => "download_platform_busy",
            Self::Io => "download_platform_io",
        })
    }
}

impl std::error::Error for PlatformError {}

#[async_trait]
pub trait PlatformDownloadHandle: Send + Sync {
    fn facts(&self) -> PlatformFacts;

    async fn verify(&self) -> Result<PlatformFacts, PlatformError>;

    async fn read_exact_chunk(&mut self, span: DownloadSpan) -> Result<Vec<u8>, PlatformError>;
}

#[async_trait]
pub trait DownloadPlatform: Send + Sync {
    /// Opens the final component for reading while denying write and delete
    /// sharing, after traversing every preceding component handle-relatively.
    async fn open_read_exclusive(
        &self,
        components: &[String],
    ) -> Result<Box<dyn PlatformDownloadHandle>, PlatformError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReconciliationRequest {
    pub project_id: ProjectId,
    pub file_id: FileEntryId,
}

pub trait ReconciliationScheduler: Send + Sync {
    /// Returns false when the bounded scheduler cannot accept more work.
    fn try_schedule(&self, request: ReconciliationRequest) -> bool;
}

pub const DEFAULT_RECONCILIATION_QUEUE_CAPACITY: usize = 256;

pub struct BoundedReconciliationScheduler {
    sender: mpsc::Sender<ReconciliationRequest>,
}

impl BoundedReconciliationScheduler {
    #[must_use]
    pub fn channel(capacity: usize) -> (Arc<Self>, mpsc::Receiver<ReconciliationRequest>) {
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        (Arc::new(Self { sender }), receiver)
    }
}

impl ReconciliationScheduler for BoundedReconciliationScheduler {
    fn try_schedule(&self, request: ReconciliationRequest) -> bool {
        self.sender.try_send(request).is_ok()
    }
}

#[derive(Clone)]
pub struct CatalogDownload {
    filename: String,
    components: Vec<String>,
    facts: PlatformFacts,
    ready_sha256: Option<[u8; 32]>,
}

#[async_trait]
pub trait DownloadCatalog: Send + Sync {
    async fn lookup(
        &self,
        project_id: ProjectId,
        file_id: FileEntryId,
    ) -> Result<CatalogDownload, DownloadError>;
}

#[derive(Clone)]
pub struct SqliteDownloadCatalog {
    pool: SqlitePool,
}

impl SqliteDownloadCatalog {
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl DownloadCatalog for SqliteDownloadCatalog {
    async fn lookup(
        &self,
        project_id: ProjectId,
        file_id: FileEntryId,
    ) -> Result<CatalogDownload, DownloadError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| DownloadError::Unavailable)?;
        let project = sqlx::query("SELECT status, deleted_at FROM project WHERE id = ?")
            .bind(project_id.to_string())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(|_| DownloadError::Unavailable)?
            .ok_or(DownloadError::ProjectNotFound)?;
        let status: String = project
            .try_get("status")
            .map_err(|_| DownloadError::Unavailable)?;
        let deleted_at: Option<String> = project
            .try_get("deleted_at")
            .map_err(|_| DownloadError::Unavailable)?;
        if deleted_at.is_some() {
            return Err(DownloadError::ProjectNotFound);
        }
        if !matches!(status.as_str(), "active" | "archived") {
            return Err(DownloadError::Unavailable);
        }

        let row = sqlx::query(
            "SELECT parent_id, exact_name, kind, platform_kind, volume_serial,
                    filesystem_file_id, size, mtime_filetime_100ns, hash,
                    hash_state, state
             FROM file_entry WHERE id = ? AND project_id = ?",
        )
        .bind(file_id.to_string())
        .bind(project_id.to_string())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|_| DownloadError::Unavailable)?
        .ok_or(DownloadError::FileNotFound)?;
        let state: String = row
            .try_get("state")
            .map_err(|_| DownloadError::Unavailable)?;
        validate_file_state(&state)?;
        let kind: String = row
            .try_get("kind")
            .map_err(|_| DownloadError::Unavailable)?;
        if kind != "file" {
            return Err(DownloadError::NotAFile);
        }
        let filename: String = row
            .try_get("exact_name")
            .map_err(|_| DownloadError::Unavailable)?;
        let mut names = vec![filename.clone()];
        let mut parent_id: Option<String> = row
            .try_get("parent_id")
            .map_err(|_| DownloadError::Unavailable)?;
        let mut visited = HashSet::new();
        while let Some(raw_parent_id) = parent_id {
            if names.len() > MAX_DIRECTORY_DEPTH {
                return Err(DownloadError::Unavailable);
            }
            let parsed: FileEntryId = raw_parent_id
                .parse()
                .map_err(|_| DownloadError::Unavailable)?;
            if parsed.to_string() != raw_parent_id || !visited.insert(parsed) {
                return Err(DownloadError::Unavailable);
            }
            let parent = sqlx::query(
                "SELECT parent_id, exact_name, kind, state FROM file_entry
                 WHERE id = ? AND project_id = ?",
            )
            .bind(&raw_parent_id)
            .bind(project_id.to_string())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(|_| DownloadError::Unavailable)?
            .ok_or(DownloadError::FileNotFound)?;
            let parent_state: String = parent
                .try_get("state")
                .map_err(|_| DownloadError::Unavailable)?;
            validate_parent_state(&parent_state)?;
            let parent_kind: String = parent
                .try_get("kind")
                .map_err(|_| DownloadError::Unavailable)?;
            if parent_kind != "directory" {
                return Err(DownloadError::Unsupported);
            }
            names.push(
                parent
                    .try_get("exact_name")
                    .map_err(|_| DownloadError::Unavailable)?,
            );
            parent_id = parent
                .try_get("parent_id")
                .map_err(|_| DownloadError::Unavailable)?;
        }
        names.reverse();

        let platform_kind: String = row
            .try_get("platform_kind")
            .map_err(|_| DownloadError::Unavailable)?;
        if platform_kind != "windows_file_id" {
            return Err(DownloadError::Unsupported);
        }
        let volume_serial: Vec<u8> = row
            .try_get("volume_serial")
            .map_err(|_| DownloadError::Unavailable)?;
        let filesystem_file_id: Vec<u8> = row
            .try_get("filesystem_file_id")
            .map_err(|_| DownloadError::Unavailable)?;
        let size: i64 = row
            .try_get("size")
            .map_err(|_| DownloadError::Unavailable)?;
        let mtime_filetime_100ns: i64 = row
            .try_get("mtime_filetime_100ns")
            .map_err(|_| DownloadError::Unavailable)?;
        if size < 0 || mtime_filetime_100ns < 0 {
            return Err(DownloadError::Unavailable);
        }
        let hash_state: String = row
            .try_get("hash_state")
            .map_err(|_| DownloadError::Unavailable)?;
        let hash: Option<Vec<u8>> = row
            .try_get("hash")
            .map_err(|_| DownloadError::Unavailable)?;
        let ready_sha256 = if hash_state == "ready" {
            Some(
                hash.ok_or(DownloadError::Unavailable)?
                    .try_into()
                    .map_err(|_| DownloadError::Unavailable)?,
            )
        } else {
            None
        };
        let mut components = vec![
            "projects".to_owned(),
            project_id.to_string(),
            "files".to_owned(),
        ];
        components.extend(names);
        transaction
            .commit()
            .await
            .map_err(|_| DownloadError::Unavailable)?;
        Ok(CatalogDownload {
            filename,
            components,
            facts: PlatformFacts {
                volume_serial: u64::from_le_bytes(
                    volume_serial
                        .try_into()
                        .map_err(|_| DownloadError::Unavailable)?,
                ),
                file_id: u128::from_le_bytes(
                    filesystem_file_id
                        .try_into()
                        .map_err(|_| DownloadError::Unavailable)?,
                ),
                length: size as u64,
                mtime_filetime_100ns,
            },
            ready_sha256,
        })
    }
}

fn validate_file_state(state: &str) -> Result<(), DownloadError> {
    match state {
        "live" => Ok(()),
        "settling" => Err(DownloadError::Settling),
        "unsupported" => Err(DownloadError::Unsupported),
        "missing" | "trashed" => Err(DownloadError::FileNotFound),
        _ => Err(DownloadError::Unavailable),
    }
}

fn validate_parent_state(state: &str) -> Result<(), DownloadError> {
    match state {
        "live" => Ok(()),
        "settling" => Err(DownloadError::Settling),
        "unsupported" => Err(DownloadError::Unsupported),
        "missing" | "trashed" => Err(DownloadError::FileNotFound),
        _ => Err(DownloadError::Unavailable),
    }
}

#[derive(Clone)]
pub struct ProductionDownloadSource {
    catalog: Arc<dyn DownloadCatalog>,
    platform: Arc<dyn DownloadPlatform>,
    scheduler: Arc<dyn ReconciliationScheduler>,
}

impl ProductionDownloadSource {
    #[must_use]
    pub fn new(
        catalog: Arc<dyn DownloadCatalog>,
        platform: Arc<dyn DownloadPlatform>,
        scheduler: Arc<dyn ReconciliationScheduler>,
    ) -> Self {
        Self {
            catalog,
            platform,
            scheduler,
        }
    }
}

#[async_trait]
impl DownloadSource for ProductionDownloadSource {
    async fn open_verified(
        &self,
        project_id: ProjectId,
        file_id: FileEntryId,
    ) -> Result<Box<dyn VerifiedDownload>, DownloadError> {
        let catalog = self.catalog.lookup(project_id, file_id).await?;
        let handle = match self.platform.open_read_exclusive(&catalog.components).await {
            Ok(handle) => handle,
            Err(PlatformError::NotFound) => {
                schedule(&*self.scheduler, project_id, file_id);
                return Err(DownloadError::IdentityChanged);
            }
            Err(PlatformError::Unsupported) => return Err(DownloadError::Unsupported),
            Err(PlatformError::Busy | PlatformError::Io) => {
                return Err(DownloadError::Unavailable);
            }
        };
        if handle.facts() != catalog.facts {
            schedule(&*self.scheduler, project_id, file_id);
            return Err(DownloadError::IdentityChanged);
        }
        let metadata =
            DownloadMetadata::new(catalog.filename, catalog.facts.length, catalog.ready_sha256)
                .map_err(|_| DownloadError::Unavailable)?;
        Ok(Box::new(ProductionVerifiedDownload {
            metadata,
            expected: catalog.facts,
            handle,
            scheduler: Arc::clone(&self.scheduler),
            project_id,
            file_id,
        }))
    }
}

struct ProductionVerifiedDownload {
    metadata: DownloadMetadata,
    expected: PlatformFacts,
    handle: Box<dyn PlatformDownloadHandle>,
    scheduler: Arc<dyn ReconciliationScheduler>,
    project_id: ProjectId,
    file_id: FileEntryId,
}

#[async_trait]
impl VerifiedDownload for ProductionVerifiedDownload {
    fn metadata(&self) -> &DownloadMetadata {
        &self.metadata
    }

    async fn verify(&self) -> Result<(), DownloadError> {
        let facts = match self.handle.verify().await {
            Ok(facts) => facts,
            Err(PlatformError::NotFound | PlatformError::Unsupported) => {
                schedule(&*self.scheduler, self.project_id, self.file_id);
                return Err(DownloadError::IdentityChanged);
            }
            Err(PlatformError::Busy | PlatformError::Io) => {
                return Err(DownloadError::Unavailable);
            }
        };
        if facts != self.expected {
            schedule(&*self.scheduler, self.project_id, self.file_id);
            return Err(DownloadError::IdentityChanged);
        }
        Ok(())
    }

    async fn read_exact_chunk(&mut self, span: DownloadSpan) -> Result<Vec<u8>, DownloadReadError> {
        self.handle
            .read_exact_chunk(span)
            .await
            .map_err(|_| DownloadReadError::Io)
    }
}

fn schedule(scheduler: &dyn ReconciliationScheduler, project_id: ProjectId, file_id: FileEntryId) {
    let _ = scheduler.try_schedule(ReconciliationRequest {
        project_id,
        file_id,
    });
}

#[derive(Clone)]
pub struct WindowsDownloadPlatform {
    storage: cellar_windows::WindowsStorage,
}

impl WindowsDownloadPlatform {
    #[must_use]
    pub fn new(storage: cellar_windows::WindowsStorage) -> Self {
        Self { storage }
    }
}

#[async_trait]
impl DownloadPlatform for WindowsDownloadPlatform {
    async fn open_read_exclusive(
        &self,
        components: &[String],
    ) -> Result<Box<dyn PlatformDownloadHandle>, PlatformError> {
        if components.is_empty() {
            return Err(PlatformError::Unsupported);
        }
        let storage = self.storage.clone();
        let components = components.to_vec();
        tokio::task::spawn_blocking(move || {
            let mut parent = storage.root().clone();
            for component in &components[..components.len() - 1] {
                let name = cellar_windows::WindowsName::parse(component)
                    .map_err(|_| PlatformError::Unsupported)?;
                parent = storage.open_verified(&parent, &name).map_err(map_storage)?;
            }
            let name = cellar_windows::WindowsName::parse(
                components.last().ok_or(PlatformError::Unsupported)?,
            )
            .map_err(|_| PlatformError::Unsupported)?;
            let handle = storage
                .open_download_verified(&parent, &name)
                .map_err(map_storage)?;
            let facts = windows_facts(storage.download_metadata(&handle).map_err(map_storage)?);
            Ok::<_, PlatformError>(Box::new(WindowsPlatformHandle {
                storage,
                handle,
                facts,
            }) as Box<dyn PlatformDownloadHandle>)
        })
        .await
        .map_err(|_| PlatformError::Io)?
    }
}

struct WindowsPlatformHandle {
    storage: cellar_windows::WindowsStorage,
    handle: cellar_windows::VerifiedHandle,
    facts: PlatformFacts,
}

#[async_trait]
impl PlatformDownloadHandle for WindowsPlatformHandle {
    fn facts(&self) -> PlatformFacts {
        self.facts
    }

    async fn verify(&self) -> Result<PlatformFacts, PlatformError> {
        let storage = self.storage.clone();
        let handle = self.handle.clone();
        tokio::task::spawn_blocking(move || {
            storage
                .download_metadata(&handle)
                .map(windows_facts)
                .map_err(map_storage)
        })
        .await
        .map_err(|_| PlatformError::Io)?
    }

    async fn read_exact_chunk(&mut self, span: DownloadSpan) -> Result<Vec<u8>, PlatformError> {
        let storage = self.storage.clone();
        let handle = self.handle.clone();
        let length = usize::try_from(span.length()).map_err(|_| PlatformError::Io)?;
        tokio::task::spawn_blocking(move || {
            storage
                .read_download_exact(&handle, span.start(), length)
                .map_err(map_storage)
        })
        .await
        .map_err(|_| PlatformError::Io)?
    }
}

fn windows_facts(metadata: cellar_windows::VerifiedFileMetadata) -> PlatformFacts {
    PlatformFacts {
        volume_serial: metadata.identity.volume_serial,
        file_id: metadata.identity.file_id,
        length: metadata.length,
        mtime_filetime_100ns: metadata.mtime_filetime_100ns,
    }
}

fn map_storage(error: cellar_windows::StorageError) -> PlatformError {
    match error.kind() {
        cellar_windows::StorageErrorKind::NotFound => PlatformError::NotFound,
        cellar_windows::StorageErrorKind::Unsupported
        | cellar_windows::StorageErrorKind::InvalidName => PlatformError::Unsupported,
        cellar_windows::StorageErrorKind::AccessDenied
        | cellar_windows::StorageErrorKind::Conflict => PlatformError::Busy,
        cellar_windows::StorageErrorKind::CleanupFailed
        | cellar_windows::StorageErrorKind::WorkerFailed
        | cellar_windows::StorageErrorKind::InsufficientStorage
        | cellar_windows::StorageErrorKind::Io => PlatformError::Io,
    }
}
