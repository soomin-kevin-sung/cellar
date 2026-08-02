use sqlx::{Executor, Sqlite, SqlitePool, Transaction};

use crate::{
    DbError,
    pool::{COLLATION_METADATA_NAME_KEY, COLLATION_METADATA_VERSION_KEY, metadata_matches},
};

struct Migration {
    version: i64,
    name: &'static str,
    fingerprint: &'static str,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial",
        fingerprint: "cellar-0001-initial-v3",
        sql: include_str!("../../../migrations/0001_initial.sql"),
    },
    Migration {
        version: 2,
        name: "indexes",
        fingerprint: "cellar-0002-indexes-v2",
        sql: include_str!("../../../migrations/0002_indexes.sql"),
    },
];

const FILE_CATALOG_EXTENSION_SQL: &str =
    include_str!("../../../migrations/expand_file_catalog_epoch.sql");
const FILE_CATALOG_EXTENSION_FINGERPRINT: &str = "cellar-file-catalog-epoch-v1";
const UPLOAD_CLEANUP_EXTENSION_SQL: &str =
    include_str!("../../../migrations/expand_upload_staging_cleanup.sql");
const UPLOAD_CLEANUP_EXTENSION_FINGERPRINT: &str = "cellar-upload-staging-cleanup-v1";
const UPLOAD_IDENTITY_EXTENSION_SQL: &str =
    include_str!("../../../migrations/expand_upload_staging_identity.sql");
const UPLOAD_IDENTITY_EXTENSION_FINGERPRINT: &str = "cellar-upload-staging-identity-v1";
const UPLOAD_FINALIZATION_EXTENSION_SQL: &str =
    include_str!("../../../migrations/expand_upload_finalization.sql");
const UPLOAD_FINALIZATION_EXTENSION_FINGERPRINT: &str = "cellar-upload-finalization-v1";
const UPLOAD_FINALIZATION_TABLE_SQL: &str = "
    CREATE TABLE upload_finalization (
      upload_id TEXT PRIMARY KEY NOT NULL,
      operation_id TEXT NOT NULL UNIQUE,
      file_entry_id TEXT NOT NULL UNIQUE,
      result_identity BLOB CHECK (
        result_identity IS NULL OR length(result_identity) = 24
      ),
      FOREIGN KEY (upload_id) REFERENCES upload_session(id) ON DELETE CASCADE,
      FOREIGN KEY (operation_id) REFERENCES operation(id) ON DELETE CASCADE
    )";
const UPLOAD_IDENTITY_TABLE_SQL: &str = "
    CREATE TABLE upload_staging_identity (
      upload_id TEXT PRIMARY KEY NOT NULL,
      platform_identity BLOB NOT NULL CHECK(length(platform_identity) = 24),
      FOREIGN KEY (upload_id) REFERENCES upload_session(id) ON DELETE CASCADE
    )";
const UPLOAD_CLEANUP_TABLE_SQL: &str = "
    CREATE TABLE upload_staging_cleanup (
      upload_id TEXT PRIMARY KEY NOT NULL,
      FOREIGN KEY (upload_id) REFERENCES upload_session(id) ON DELETE CASCADE
    )";
const UPLOAD_CLEANUP_TRIGGER_SQL: &str = "
    CREATE TRIGGER upload_staging_cleanup_terminal
    AFTER UPDATE OF state ON upload_session
    WHEN NEW.state IN ('failed', 'cancelled')
     AND OLD.state NOT IN ('failed', 'cancelled')
    BEGIN
      INSERT OR IGNORE INTO upload_staging_cleanup (upload_id) VALUES (NEW.id);
    END";
const FILE_CATALOG_OBJECTS: &[(&str, &str)] = &[
    ("table", "cellar_schema_extension"),
    ("table", "file_catalog_epoch"),
    ("trigger", "file_catalog_epoch_project_insert"),
    ("trigger", "file_catalog_epoch_file_insert"),
    ("trigger", "file_catalog_epoch_file_update_same_project"),
    ("trigger", "file_catalog_epoch_file_update_old_project"),
    ("trigger", "file_catalog_epoch_file_delete"),
    ("index", "ix_file_list_root"),
    ("index", "ix_file_list_child"),
];

/// Applies Cellar's expand-only migrations under an exclusive SQLite
/// transaction and rejects non-prefix or future migration histories.
pub async fn migrate(pool: &SqlitePool) -> Result<(), DbError> {
    let mut transaction = pool
        .begin_with("BEGIN EXCLUSIVE")
        .await
        .map_err(DbError::Migration)?;

    let result = migrate_locked(&mut transaction).await;
    match result {
        Ok(()) => transaction.commit().await.map_err(DbError::Migration),
        Err(error) => {
            transaction.rollback().await.map_err(DbError::Migration)?;
            Err(error)
        }
    }
}

async fn migrate_locked(transaction: &mut Transaction<'_, Sqlite>) -> Result<(), DbError> {
    transaction
        .execute(
            "CREATE TABLE IF NOT EXISTS cellar_schema_migration (
               version INTEGER PRIMARY KEY NOT NULL,
               name TEXT NOT NULL,
               fingerprint TEXT NOT NULL
             )",
        )
        .await
        .map_err(DbError::Migration)?;

    let installed: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT version, name, fingerprint
         FROM cellar_schema_migration
         ORDER BY version",
    )
    .fetch_all(&mut **transaction)
    .await
    .map_err(DbError::Migration)?;

    if installed.len() > MIGRATIONS.len()
        || installed
            .iter()
            .zip(MIGRATIONS)
            .any(|((version, name, fingerprint), expected)| {
                *version != expected.version
                    || name != expected.name
                    || fingerprint != expected.fingerprint
            })
    {
        return Err(DbError::SchemaVersion);
    }

    for migration in &MIGRATIONS[installed.len()..] {
        sqlx::raw_sql(migration.sql)
            .execute(&mut **transaction)
            .await
            .map_err(DbError::Migration)?;
        sqlx::query(
            "INSERT INTO cellar_schema_migration (version, name, fingerprint)
             VALUES (?, ?, ?)",
        )
        .bind(migration.version)
        .bind(migration.name)
        .bind(migration.fingerprint)
        .execute(&mut **transaction)
        .await
        .map_err(DbError::Migration)?;
    }

    sqlx::raw_sql(FILE_CATALOG_EXTENSION_SQL)
        .execute(&mut **transaction)
        .await
        .map_err(DbError::Migration)?;
    validate_file_catalog_extension(transaction).await?;
    let upload_cleanup_marker: Option<String> = sqlx::query_scalar(
        "SELECT fingerprint FROM cellar_schema_extension
         WHERE name = 'upload_staging_cleanup'",
    )
    .fetch_optional(&mut **transaction)
    .await
    .map_err(DbError::Migration)?;
    if upload_cleanup_marker.is_some() {
        validate_upload_cleanup_extension(transaction).await?;
    } else {
        sqlx::raw_sql(UPLOAD_CLEANUP_EXTENSION_SQL)
            .execute(&mut **transaction)
            .await
            .map_err(DbError::Migration)?;
        validate_upload_cleanup_extension(transaction).await?;
    }
    let upload_identity_marker: Option<String> = sqlx::query_scalar(
        "SELECT fingerprint FROM cellar_schema_extension
         WHERE name = 'upload_staging_identity'",
    )
    .fetch_optional(&mut **transaction)
    .await
    .map_err(DbError::Migration)?;
    if upload_identity_marker.is_none() {
        sqlx::raw_sql(UPLOAD_IDENTITY_EXTENSION_SQL)
            .execute(&mut **transaction)
            .await
            .map_err(DbError::Migration)?;
    }
    validate_upload_identity_extension(transaction).await?;
    let upload_finalization_marker: Option<String> = sqlx::query_scalar(
        "SELECT fingerprint FROM cellar_schema_extension
         WHERE name = 'upload_finalization'",
    )
    .fetch_optional(&mut **transaction)
    .await
    .map_err(DbError::Migration)?;
    if upload_finalization_marker.is_none() {
        sqlx::raw_sql(UPLOAD_FINALIZATION_EXTENSION_SQL)
            .execute(&mut **transaction)
            .await
            .map_err(DbError::Migration)?;
    }
    validate_upload_finalization_extension(transaction).await?;

    let metadata: Vec<(String, String)> = sqlx::query_as(
        "SELECT key, value FROM cellar_schema_metadata
         WHERE key IN (?, ?) ORDER BY key",
    )
    .bind(COLLATION_METADATA_NAME_KEY)
    .bind(COLLATION_METADATA_VERSION_KEY)
    .fetch_all(&mut **transaction)
    .await
    .map_err(DbError::Migration)?;
    if !metadata_matches(&metadata) {
        return Err(DbError::SchemaVersion);
    }

    Ok(())
}

async fn validate_upload_finalization_extension(
    transaction: &mut Transaction<'_, Sqlite>,
) -> Result<(), DbError> {
    let fingerprint: Option<String> = sqlx::query_scalar(
        "SELECT fingerprint FROM cellar_schema_extension
         WHERE name = 'upload_finalization'",
    )
    .fetch_optional(&mut **transaction)
    .await
    .map_err(DbError::Migration)?;
    if fingerprint.as_deref() != Some(UPLOAD_FINALIZATION_EXTENSION_FINGERPRINT) {
        return Err(DbError::SchemaVersion);
    }
    let definition: Option<String> = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master
         WHERE type = 'table' AND name = 'upload_finalization'",
    )
    .fetch_optional(&mut **transaction)
    .await
    .map_err(DbError::Migration)?;
    if !definition.is_some_and(|sql| sql_eq(&sql, UPLOAD_FINALIZATION_TABLE_SQL)) {
        return Err(DbError::SchemaVersion);
    }
    Ok(())
}

async fn validate_upload_identity_extension(
    transaction: &mut Transaction<'_, Sqlite>,
) -> Result<(), DbError> {
    let fingerprint: Option<String> = sqlx::query_scalar(
        "SELECT fingerprint FROM cellar_schema_extension
         WHERE name = 'upload_staging_identity'",
    )
    .fetch_optional(&mut **transaction)
    .await
    .map_err(DbError::Migration)?;
    if fingerprint.as_deref() != Some(UPLOAD_IDENTITY_EXTENSION_FINGERPRINT) {
        return Err(DbError::SchemaVersion);
    }
    let definition: Option<String> = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master
         WHERE type = 'table' AND name = 'upload_staging_identity'",
    )
    .fetch_optional(&mut **transaction)
    .await
    .map_err(DbError::Migration)?;
    if !definition.is_some_and(|sql| sql_eq(&sql, UPLOAD_IDENTITY_TABLE_SQL)) {
        return Err(DbError::SchemaVersion);
    }
    Ok(())
}

async fn validate_upload_cleanup_extension(
    transaction: &mut Transaction<'_, Sqlite>,
) -> Result<(), DbError> {
    let fingerprint: Option<String> = sqlx::query_scalar(
        "SELECT fingerprint FROM cellar_schema_extension
         WHERE name = 'upload_staging_cleanup'",
    )
    .fetch_optional(&mut **transaction)
    .await
    .map_err(DbError::Migration)?;
    if fingerprint.as_deref() != Some(UPLOAD_CLEANUP_EXTENSION_FINGERPRINT) {
        return Err(DbError::SchemaVersion);
    }
    let objects: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sqlite_master
         WHERE (type = 'table' AND name = 'upload_staging_cleanup')
            OR (type = 'trigger' AND name = 'upload_staging_cleanup_terminal')",
    )
    .fetch_one(&mut **transaction)
    .await
    .map_err(DbError::Migration)?;
    if objects != 2 {
        return Err(DbError::SchemaVersion);
    }
    let definitions: Vec<(String, String)> = sqlx::query_as(
        "SELECT name, sql FROM sqlite_master
         WHERE name IN ('upload_staging_cleanup', 'upload_staging_cleanup_terminal')
         ORDER BY name",
    )
    .fetch_all(&mut **transaction)
    .await
    .map_err(DbError::Migration)?;
    let expected = [
        ("upload_staging_cleanup", UPLOAD_CLEANUP_TABLE_SQL),
        (
            "upload_staging_cleanup_terminal",
            UPLOAD_CLEANUP_TRIGGER_SQL,
        ),
    ];
    if expected.iter().any(|(name, sql)| {
        !definitions
            .iter()
            .any(|(actual_name, actual_sql)| actual_name == name && sql_eq(actual_sql, sql))
    }) {
        return Err(DbError::SchemaVersion);
    }
    Ok(())
}

fn sql_eq(left: &str, right: &str) -> bool {
    canonical_sql(left) == canonical_sql(right)
}

fn canonical_sql(sql: &str) -> String {
    let mut canonical = String::with_capacity(sql.len());
    let mut characters = sql.chars().peekable();
    let mut quoted_end = None;
    while let Some(character) = characters.next() {
        if let Some(end) = quoted_end {
            canonical.push(character);
            if character == end {
                if end != ']' && characters.peek() == Some(&end) {
                    canonical.push(characters.next().expect("peeked quoted escape"));
                } else {
                    quoted_end = None;
                }
            }
            continue;
        }
        match character {
            '\'' | '"' | '`' => {
                canonical.push(character);
                quoted_end = Some(character);
            }
            '[' => {
                canonical.push(character);
                quoted_end = Some(']');
            }
            character if character.is_ascii_whitespace() => {}
            character => canonical.extend(character.to_lowercase()),
        }
    }
    canonical
}

async fn validate_file_catalog_extension(
    transaction: &mut Transaction<'_, Sqlite>,
) -> Result<(), DbError> {
    let fingerprint: Option<String> = sqlx::query_scalar(
        "SELECT fingerprint FROM cellar_schema_extension
         WHERE name = 'file_catalog_epoch'",
    )
    .fetch_optional(&mut **transaction)
    .await
    .map_err(DbError::Migration)?;
    if fingerprint.as_deref() != Some(FILE_CATALOG_EXTENSION_FINGERPRINT) {
        return Err(DbError::SchemaVersion);
    }

    let objects: Vec<(String, String)> = sqlx::query_as(
        "SELECT type, name FROM sqlite_master
         WHERE name LIKE 'file_catalog_epoch_%'
            OR name IN ('cellar_schema_extension', 'file_catalog_epoch',
                        'ix_file_list_root', 'ix_file_list_child')",
    )
    .fetch_all(&mut **transaction)
    .await
    .map_err(DbError::Migration)?;
    if FILE_CATALOG_OBJECTS.iter().any(|expected| {
        !objects
            .iter()
            .any(|actual| (actual.0.as_str(), actual.1.as_str()) == *expected)
    }) {
        return Err(DbError::SchemaVersion);
    }

    let missing_epochs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM project AS p
         LEFT JOIN file_catalog_epoch AS e ON e.project_id = p.id
         WHERE e.project_id IS NULL",
    )
    .fetch_one(&mut **transaction)
    .await
    .map_err(DbError::Migration)?;
    if missing_epochs != 0 {
        return Err(DbError::SchemaVersion);
    }
    Ok(())
}
