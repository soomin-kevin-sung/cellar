use sqlx::{Executor, Sqlite, SqlitePool, Transaction};

use crate::DbError;

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
        fingerprint: "cellar-0001-initial-v2",
        sql: include_str!("../../../migrations/0001_initial.sql"),
    },
    Migration {
        version: 2,
        name: "indexes",
        fingerprint: "cellar-0002-indexes-v1",
        sql: include_str!("../../../migrations/0002_indexes.sql"),
    },
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

    Ok(())
}
