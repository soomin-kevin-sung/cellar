mod file_repo;
mod migrate;
mod operation_repo;
mod pool;
mod project_repo;
mod upload_repo;

use thiserror::Error;

pub use file_repo::SqliteFileRepository;
pub use migrate::migrate;
pub use operation_repo::{SqliteOperationRepository, decode_upload_commit_payload};
pub use pool::{FilenameCollation, open_pool};
pub use project_repo::{
    MAX_PROJECT_CREATE_PAYLOAD_BYTES, RecoveredProjectCreate, SqliteProjectRepository,
    decode_project_create_payload,
};
pub use upload_repo::SqliteUploadRepository;

/// Stable, client-safe database failure categories.
///
/// The underlying sources are available through [`std::error::Error::source`]
/// for diagnostics, while `Display` deliberately contains no SQL or host path.
#[derive(Debug, Error)]
pub enum DbError {
    #[error("database connection failed")]
    Connection(#[source] sqlx::Error),
    #[error("database migration failed")]
    Migration(#[source] sqlx::Error),
    #[error("database schema version is unsupported")]
    SchemaVersion,
}
