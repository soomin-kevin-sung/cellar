use std::{cmp::Ordering, path::Path, sync::Arc, time::Duration};

use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};

use crate::DbError;

/// Caller-supplied filename comparator registered as `WINDOWS_ORDINAL_CI`.
///
/// Cellar's Windows integration supplies a comparator backed by
/// `CompareStringOrdinal(..., TRUE)`. Keeping the callback here avoids a
/// persistence-to-platform dependency and prevents a Unicode approximation
/// from becoming an accidental database invariant.
pub type FilenameComparator = Arc<dyn Fn(&str, &str) -> Ordering + Send + Sync + 'static>;

/// Opens Cellar's local SQLite pool and registers its filename collation on
/// every connection.
pub async fn open_pool(
    path: impl AsRef<Path>,
    filename_comparator: FilenameComparator,
) -> Result<SqlitePool, DbError> {
    let comparator = Arc::clone(&filename_comparator);
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Full)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5))
        .collation("WINDOWS_ORDINAL_CI", move |left, right| {
            comparator(left, right)
        });

    SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await
        .map_err(DbError::Connection)
}
