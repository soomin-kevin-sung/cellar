mod migrate;
mod pool;
mod project_repo;

use thiserror::Error;

pub use migrate::migrate;
pub use pool::{FilenameCollation, open_pool};
pub use project_repo::{
    MAX_PROJECT_CREATE_PAYLOAD_BYTES, RecoveredProjectCreate, SqliteProjectRepository,
    decode_project_create_payload,
};

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
