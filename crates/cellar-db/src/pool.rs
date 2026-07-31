use std::{
    cmp::Ordering,
    ffi::{CStr, c_int, c_void},
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    slice, str,
    sync::Arc,
    time::Duration,
};

use libsqlite3_sys::{SQLITE_OK, SQLITE_UTF8, sqlite3, sqlite3_create_collation_v2};
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};

use crate::DbError;

pub(crate) const COLLATION_METADATA_NAME_KEY: &str = "filename_collation_name";
pub(crate) const COLLATION_METADATA_VERSION_KEY: &str = "filename_collation_version";
pub(crate) const WINDOWS_ORDINAL_CI_V1_NAME: &str = "WINDOWS_ORDINAL_CI_V1";
pub(crate) const WINDOWS_ORDINAL_CI_V1_VERSION: u32 = 1;
const WINDOWS_ORDINAL_CI_V1_C_NAME: &CStr = c"WINDOWS_ORDINAL_CI_V1";

type Comparator = dyn Fn(&str, &str) -> Ordering + Send + Sync + 'static;

/// Opaque registration descriptor for Cellar's version-one Windows filename
/// comparison contract.
///
/// The caller supplies the platform implementation of
/// `CompareStringOrdinal(..., TRUE)`. The name and semantic version cannot be
/// changed independently of this crate's migration and index-rebuild policy.
#[derive(Clone)]
pub struct FilenameCollation {
    name: &'static CStr,
    semantic_version: u32,
    comparator: Arc<Comparator>,
}

impl FilenameCollation {
    /// Binds an exact platform comparator to the version-one database contract.
    pub fn windows_ordinal_ci_v1(
        comparator: impl Fn(&str, &str) -> Ordering + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: WINDOWS_ORDINAL_CI_V1_C_NAME,
            semantic_version: WINDOWS_ORDINAL_CI_V1_VERSION,
            comparator: Arc::new(comparator),
        }
    }
}

struct CollationContext {
    comparator: Arc<Comparator>,
}

/// Opens Cellar's local SQLite pool and registers its versioned filename
/// collation on every connection.
pub async fn open_pool(
    path: impl AsRef<Path>,
    filename_collation: FilenameCollation,
) -> Result<SqlitePool, DbError> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Full)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));
    let connection_collation = filename_collation.clone();

    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .after_connect(move |connection, _metadata| {
            let collation = connection_collation.clone();
            Box::pin(async move {
                let mut handle = connection.lock_handle().await?;
                register_collation(handle.as_raw_handle().as_ptr(), collation)
            })
        })
        .connect_with(options)
        .await
        .map_err(DbError::Connection)?;

    if filename_collation.name != WINDOWS_ORDINAL_CI_V1_C_NAME
        || filename_collation.semantic_version != WINDOWS_ORDINAL_CI_V1_VERSION
        || !open_schema_uses_expected_collation(&pool).await?
    {
        pool.close().await;
        return Err(DbError::SchemaVersion);
    }

    Ok(pool)
}

async fn open_schema_uses_expected_collation(pool: &SqlitePool) -> Result<bool, DbError> {
    let metadata_exists: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sqlite_master
         WHERE type = 'table' AND name = 'cellar_schema_metadata'",
    )
    .fetch_one(pool)
    .await
    .map_err(DbError::Connection)?;
    if metadata_exists == 0 {
        return Ok(true);
    }

    let metadata: Vec<(String, String)> = sqlx::query_as(
        "SELECT key, value FROM cellar_schema_metadata
         WHERE key IN (?, ?) ORDER BY key",
    )
    .bind(COLLATION_METADATA_NAME_KEY)
    .bind(COLLATION_METADATA_VERSION_KEY)
    .fetch_all(pool)
    .await
    .map_err(DbError::Connection)?;
    Ok(metadata_matches(&metadata))
}

pub(crate) fn metadata_matches(metadata: &[(String, String)]) -> bool {
    metadata.len() == 2
        && metadata.iter().any(|(key, value)| {
            key == COLLATION_METADATA_NAME_KEY && value == WINDOWS_ORDINAL_CI_V1_NAME
        })
        && metadata.iter().any(|(key, value)| {
            key == COLLATION_METADATA_VERSION_KEY
                && value == &WINDOWS_ORDINAL_CI_V1_VERSION.to_string()
        })
}

fn register_collation(
    handle: *mut sqlite3,
    collation: FilenameCollation,
) -> Result<(), sqlx::Error> {
    let context = Box::into_raw(Box::new(CollationContext {
        comparator: collation.comparator,
    }));
    // SAFETY: `handle` is borrowed from SQLx's locked handle for the duration
    // of this call. `context` is a valid boxed allocation transferred to
    // SQLite on success, and both callbacks use its exact concrete type.
    let result = unsafe {
        sqlite3_create_collation_v2(
            handle,
            collation.name.as_ptr(),
            SQLITE_UTF8,
            context.cast(),
            Some(compare_collation),
            Some(destroy_collation),
        )
    };
    if result == SQLITE_OK {
        return Ok(());
    }

    // SQLite does not invoke xDestroy when registration fails. Reclaim the
    // allocation here, catching even a pathological comparator destructor so
    // no Rust panic escapes this registration boundary.
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: registration failed, so ownership was not transferred and
        // `context` still points to the unique Box allocated above.
        unsafe { drop(Box::from_raw(context)) };
    }));
    Err(sqlx::Error::Protocol(
        "filename collation registration failed".into(),
    ))
}

unsafe extern "C" fn compare_collation(
    context: *mut c_void,
    left_len: c_int,
    left_ptr: *const c_void,
    right_len: c_int,
    right_ptr: *const c_void,
) -> c_int {
    if context.is_null() {
        return ordering_to_sqlite(left_len.cmp(&right_len));
    }

    // SAFETY: SQLite calls a UTF-8 collation with buffers valid for the stated
    // non-negative lengths for the duration of the callback. The helper also
    // rejects negative lengths and null pointers before constructing slices.
    let Some(left) = (unsafe { sqlite_bytes(left_ptr, left_len) }) else {
        return ordering_to_sqlite(left_len.cmp(&right_len));
    };
    // SAFETY: same SQLite callback contract as for `left` above.
    let Some(right) = (unsafe { sqlite_bytes(right_ptr, right_len) }) else {
        return ordering_to_sqlite(left_len.cmp(&right_len));
    };
    let raw_fallback = ordering_to_sqlite(left.cmp(right));

    let (Ok(left), Ok(right)) = (str::from_utf8(left), str::from_utf8(right)) else {
        return raw_fallback;
    };
    // SAFETY: SQLite retains the boxed context until it invokes xDestroy, and
    // serializes use of this connection while SQLx's worker owns the handle.
    let comparator = unsafe { &*context.cast::<CollationContext>() };
    match catch_unwind(AssertUnwindSafe(|| (comparator.comparator)(left, right))) {
        Ok(ordering) => ordering_to_sqlite(ordering),
        Err(_) => raw_fallback,
    }
}

unsafe fn sqlite_bytes<'a>(pointer: *const c_void, length: c_int) -> Option<&'a [u8]> {
    let length = usize::try_from(length).ok()?;
    if length == 0 {
        return Some(&[]);
    }
    if pointer.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees SQLite supplied a readable buffer of
    // exactly `length` bytes for the duration represented by `'a`.
    Some(unsafe { slice::from_raw_parts(pointer.cast(), length) })
}

unsafe extern "C" fn destroy_collation(context: *mut c_void) {
    if context.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: SQLite calls xDestroy exactly once for the context whose Box
        // ownership was transferred by successful registration.
        unsafe { drop(Box::from_raw(context.cast::<CollationContext>())) };
    }));
}

const fn ordering_to_sqlite(ordering: Ordering) -> c_int {
    match ordering {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
}
