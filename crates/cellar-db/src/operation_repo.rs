use std::collections::HashSet;

use async_trait::async_trait;
use cellar_core::{
    FileEntry, FileEntryId, FileExactName, FileHashState, FileKind, FileState,
    MAX_UPLOAD_COMMIT_COMPONENTS, MAX_UPLOAD_COMMIT_PAYLOAD_BYTES, OperationId, PlatformIdentity,
    ProjectId, PublishedUpload, StagingIdentity, UPLOAD_COMMIT_PAYLOAD_VERSION, UploadCommitIntent,
    UploadCommitPayload, UploadFinalizeRepository, UploadFinalizeRepositoryError,
    UploadFinalizeStart, UploadId, VerifiedUpload, is_safe_upload_name,
};
use serde_json::Value;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, Sqlite, SqlitePool, Transaction};
use time::{OffsetDateTime, PrimitiveDateTime, UtcOffset};

const FIXED_UTC_TIMESTAMP_FORMAT: &str =
    "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:9]Z";
const MAX_DIRECTORY_DEPTH: usize = 256;

#[derive(Clone)]
pub struct SqliteOperationRepository {
    pool: SqlitePool,
}

impl SqliteOperationRepository {
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl UploadFinalizeRepository for SqliteOperationRepository {
    async fn upload_commit(
        &self,
        upload_id: UploadId,
    ) -> Result<Option<UploadFinalizeStart>, UploadFinalizeRepositoryError> {
        let row = sqlx::query(
            "SELECT f.operation_id, f.file_entry_id, f.result_identity,
                    o.state, o.payload_version, o.payload,
                    u.state AS upload_state
             FROM upload_finalization AS f
             JOIN operation AS o ON o.id = f.operation_id
             JOIN upload_session AS u ON u.id = f.upload_id
             WHERE f.upload_id = ?",
        )
        .bind(upload_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sql)?;
        let Some(row) = row else {
            let state: Option<String> =
                sqlx::query_scalar("SELECT state FROM upload_session WHERE id = ?")
                    .bind(upload_id.to_string())
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(map_sql)?;
            return match state.as_deref() {
                None => Err(UploadFinalizeRepositoryError::NotFound),
                Some("created" | "uploading" | "verifying") => Ok(None),
                Some("complete") => Err(UploadFinalizeRepositoryError::Unavailable),
                Some("committing") => Err(UploadFinalizeRepositoryError::Unavailable),
                Some("failed" | "cancelled") => Err(UploadFinalizeRepositoryError::NotFound),
                _ => Err(UploadFinalizeRepositoryError::Unavailable),
            };
        };
        let operation_id = canonical_id::<OperationId>(
            &row.try_get::<String, _>("operation_id").map_err(map_sql)?,
        )?;
        let state: String = row.try_get("state").map_err(map_sql)?;
        let upload_state: String = row.try_get("upload_state").map_err(map_sql)?;
        let payload_version: i64 = row.try_get("payload_version").map_err(map_sql)?;
        let payload: String = row.try_get("payload").map_err(map_sql)?;
        let intent = decode_upload_commit_payload(operation_id, payload_version, &payload)?;
        if intent.upload_id != upload_id {
            return Err(UploadFinalizeRepositoryError::Unavailable);
        }
        match (state.as_str(), upload_state.as_str()) {
            ("pending" | "fs_applied", "committing") => {
                Ok(Some(UploadFinalizeStart::Intent(intent)))
            }
            ("complete", "complete") => {
                let entry_id = canonical_id::<FileEntryId>(
                    &row.try_get::<String, _>("file_entry_id").map_err(map_sql)?,
                )?;
                if entry_id != intent.file_entry_id {
                    return Err(UploadFinalizeRepositoryError::Unavailable);
                }
                let result_identity = row
                    .try_get::<Option<Vec<u8>>, _>("result_identity")
                    .map_err(map_sql)?
                    .map(|bytes| {
                        bytes
                            .try_into()
                            .map(StagingIdentity::new)
                            .map_err(|_| UploadFinalizeRepositoryError::Unavailable)
                    })
                    .transpose()?;
                if result_identity != intent.result_identity
                    || result_identity != Some(intent.staging_identity)
                {
                    return Err(UploadFinalizeRepositoryError::Unavailable);
                }
                let mut entry = read_file_entry(&self.pool, entry_id).await?;
                entry.relative_path = intent_relative_path(&intent);
                Ok(Some(UploadFinalizeStart::Completed(entry)))
            }
            ("failed", "failed") => Err(UploadFinalizeRepositoryError::Conflict),
            _ => Err(UploadFinalizeRepositoryError::Unavailable),
        }
    }

    async fn prepare_upload_commit(
        &self,
        upload_id: UploadId,
        verified: VerifiedUpload,
        now: OffsetDateTime,
    ) -> Result<UploadFinalizeStart, UploadFinalizeRepositoryError> {
        if verified.size < 0 {
            return Err(UploadFinalizeRepositoryError::Unavailable);
        }
        if let Some(existing) = self.upload_commit(upload_id).await? {
            return Ok(existing);
        }
        let mut transaction = self.pool.begin().await.map_err(map_sql)?;
        let row = sqlx::query("SELECT * FROM upload_session WHERE id = ?")
            .bind(upload_id.to_string())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sql)?
            .ok_or(UploadFinalizeRepositoryError::NotFound)?;
        let project_id =
            canonical_id::<ProjectId>(&row.try_get::<String, _>("project_id").map_err(map_sql)?)?;
        let parent_id = row
            .try_get::<Option<String>, _>("destination_parent_id")
            .map_err(map_sql)?
            .map(|value| canonical_id::<FileEntryId>(&value))
            .transpose()?;
        let destination_name = row
            .try_get::<String, _>("destination_name")
            .map_err(map_sql)?;
        FileExactName::parse(destination_name.clone())
            .map_err(|_| UploadFinalizeRepositoryError::Conflict)?;
        if !is_safe_upload_name(&destination_name) {
            return Err(UploadFinalizeRepositoryError::Conflict);
        }
        let expected_size: i64 = row.try_get("expected_size").map_err(map_sql)?;
        let committed_offset: i64 = row.try_get("committed_offset").map_err(map_sql)?;
        let state: String = row.try_get("state").map_err(map_sql)?;
        let pending_offset: Option<i64> = row.try_get("pending_offset").map_err(map_sql)?;
        let expected_hash = digest(
            row.try_get::<Option<Vec<u8>>, _>("expected_hash")
                .map_err(map_sql)?,
        )?;
        if !matches!(state.as_str(), "created" | "uploading" | "verifying")
            || pending_offset.is_some()
            || committed_offset != expected_size
            || verified.size != expected_size
            || expected_hash.is_some_and(|expected| expected != verified.sha256)
        {
            return Err(UploadFinalizeRepositoryError::Conflict);
        }
        let expected_identity: Option<Vec<u8>> = sqlx::query_scalar(
            "SELECT platform_identity FROM upload_staging_identity WHERE upload_id = ?",
        )
        .bind(upload_id.to_string())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(map_sql)?;
        if expected_identity.as_deref() != Some(&verified.staging_identity.as_bytes()) {
            return Err(UploadFinalizeRepositoryError::Unavailable);
        }
        validate_active_project(&mut transaction, project_id).await?;
        let parent = parent_facts(&mut transaction, project_id, parent_id).await?;
        ensure_destination_absent(&mut transaction, project_id, parent_id, &destination_name)
            .await?;

        let operation_id = OperationId::new();
        let file_entry_id = FileEntryId::new();
        let intent = UploadCommitIntent {
            operation_id,
            upload_id,
            project_id,
            destination_parent_id: parent_id,
            destination_parent_revision: parent.revision,
            destination_parent_identity: parent.identity,
            destination_components: parent.components,
            destination_name,
            file_entry_id,
            expected_size,
            sha256: verified.sha256,
            staging_identity: verified.staging_identity,
            result_identity: None,
        };
        let payload = encode_upload_commit_payload(&intent)?;
        let timestamp = timestamp(now)?;
        sqlx::query(
            "INSERT INTO operation
             (id, project_id, kind, state, payload_version, payload, error, created_at, updated_at)
             VALUES (?, ?, 'upload_finalize', 'pending', ?, ?, NULL, ?, ?)",
        )
        .bind(operation_id.to_string())
        .bind(project_id.to_string())
        .bind(UPLOAD_COMMIT_PAYLOAD_VERSION)
        .bind(payload)
        .bind(&timestamp)
        .bind(&timestamp)
        .execute(&mut *transaction)
        .await
        .map_err(map_constraint)?;
        sqlx::query(
            "INSERT INTO upload_finalization
             (upload_id, operation_id, file_entry_id, result_identity)
             VALUES (?, ?, ?, NULL)",
        )
        .bind(upload_id.to_string())
        .bind(operation_id.to_string())
        .bind(file_entry_id.to_string())
        .execute(&mut *transaction)
        .await
        .map_err(map_constraint)?;
        let updated = sqlx::query(
            "UPDATE upload_session SET state = 'committing'
             WHERE id = ? AND state IN ('created', 'uploading', 'verifying')
               AND committed_offset = expected_size AND pending_offset IS NULL",
        )
        .bind(upload_id.to_string())
        .execute(&mut *transaction)
        .await
        .map_err(map_sql)?
        .rows_affected();
        if updated != 1 {
            return Err(UploadFinalizeRepositoryError::Conflict);
        }
        transaction.commit().await.map_err(map_sql)?;
        Ok(UploadFinalizeStart::Intent(intent))
    }

    async fn mark_upload_fs_applied(
        &self,
        intent: &UploadCommitIntent,
        published: PublishedUpload,
        now: OffsetDateTime,
    ) -> Result<UploadCommitIntent, UploadFinalizeRepositoryError> {
        if published.identity != intent.staging_identity || published.size != intent.expected_size {
            return Err(UploadFinalizeRepositoryError::Conflict);
        }
        let mut applied = intent.clone();
        applied.result_identity = Some(published.identity);
        let payload = encode_upload_commit_payload(&applied)?;
        let affected = sqlx::query(
            "UPDATE operation SET state = 'fs_applied', payload = ?, updated_at = ?
             WHERE id = ? AND kind = 'upload_finalize' AND state IN ('pending', 'fs_applied')",
        )
        .bind(payload)
        .bind(timestamp(now)?)
        .bind(intent.operation_id.to_string())
        .execute(&self.pool)
        .await
        .map_err(map_sql)?
        .rows_affected();
        if affected == 1 {
            Ok(applied)
        } else {
            match self.upload_commit(intent.upload_id).await? {
                Some(UploadFinalizeStart::Completed(_)) => Ok(applied),
                _ => Err(UploadFinalizeRepositoryError::Unavailable),
            }
        }
    }

    async fn complete_upload_commit(
        &self,
        intent: &UploadCommitIntent,
        published: PublishedUpload,
        now: OffsetDateTime,
    ) -> Result<FileEntry, UploadFinalizeRepositoryError> {
        if published.identity != intent.staging_identity
            || published.size != intent.expected_size
            || published.mtime_filetime_100ns < 0
            || intent.result_identity != Some(published.identity)
        {
            return Err(UploadFinalizeRepositoryError::Conflict);
        }
        if let Some(UploadFinalizeStart::Completed(entry)) =
            self.upload_commit(intent.upload_id).await?
        {
            return Ok(entry);
        }
        let mut transaction = self.pool.begin().await.map_err(map_sql)?;
        validate_intent_row(&mut transaction, intent).await?;
        let parent = parent_facts(
            &mut transaction,
            intent.project_id,
            intent.destination_parent_id,
        )
        .await?;
        if parent.components != intent.destination_components
            || parent.revision != intent.destination_parent_revision
            || parent.identity != intent.destination_parent_identity
        {
            return Err(UploadFinalizeRepositoryError::Conflict);
        }
        ensure_destination_absent(
            &mut transaction,
            intent.project_id,
            intent.destination_parent_id,
            &intent.destination_name,
        )
        .await?;
        let identity = published.identity.as_bytes();
        let observed_at = timestamp(now)?;
        sqlx::query(
            "INSERT INTO file_entry
             (id, project_id, parent_id, exact_name, kind, platform_kind,
              volume_serial, filesystem_file_id, size, mtime_filetime_100ns,
              hash, hash_state, state, revision, scan_generation, observed_at)
             VALUES (?, ?, ?, ?, 'file', 'windows_file_id', ?, ?, ?, ?, ?, 'ready',
                     'live', 1, 0, ?)",
        )
        .bind(intent.file_entry_id.to_string())
        .bind(intent.project_id.to_string())
        .bind(intent.destination_parent_id.map(|id| id.to_string()))
        .bind(&intent.destination_name)
        .bind(identity[..8].to_vec())
        .bind(identity[8..].to_vec())
        .bind(published.size)
        .bind(published.mtime_filetime_100ns)
        .bind(intent.sha256.to_vec())
        .bind(&observed_at)
        .execute(&mut *transaction)
        .await
        .map_err(map_constraint)?;
        let upload = sqlx::query(
            "UPDATE upload_session SET state = 'complete'
             WHERE id = ? AND state = 'committing'",
        )
        .bind(intent.upload_id.to_string())
        .execute(&mut *transaction)
        .await
        .map_err(map_sql)?
        .rows_affected();
        let operation = sqlx::query(
            "UPDATE operation SET state = 'complete', updated_at = ?
             WHERE id = ? AND kind = 'upload_finalize' AND state = 'fs_applied'",
        )
        .bind(&observed_at)
        .bind(intent.operation_id.to_string())
        .execute(&mut *transaction)
        .await
        .map_err(map_sql)?
        .rows_affected();
        let mapping = sqlx::query(
            "UPDATE upload_finalization SET result_identity = ?
             WHERE upload_id = ? AND operation_id = ? AND file_entry_id = ?",
        )
        .bind(identity.to_vec())
        .bind(intent.upload_id.to_string())
        .bind(intent.operation_id.to_string())
        .bind(intent.file_entry_id.to_string())
        .execute(&mut *transaction)
        .await
        .map_err(map_sql)?
        .rows_affected();
        if upload != 1 || operation != 1 || mapping != 1 {
            return Err(UploadFinalizeRepositoryError::Unavailable);
        }
        transaction.commit().await.map_err(map_sql)?;
        let mut entry = read_file_entry(&self.pool, intent.file_entry_id).await?;
        entry.relative_path = intent_relative_path(intent);
        Ok(entry)
    }

    async fn pending_upload_commits(
        &self,
    ) -> Result<Vec<UploadCommitIntent>, UploadFinalizeRepositoryError> {
        let rows = sqlx::query(
            "SELECT o.id, o.project_id, o.payload_version, o.payload
             FROM operation AS o
             JOIN upload_finalization AS f ON f.operation_id = o.id
             WHERE o.kind = 'upload_finalize' AND o.state IN ('pending', 'fs_applied')
             ORDER BY o.id COLLATE BINARY",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(map_sql)?;
        rows.into_iter()
            .map(|row| {
                let operation_id =
                    canonical_id::<OperationId>(&row.try_get::<String, _>("id").map_err(map_sql)?)?;
                let project_id = canonical_id::<ProjectId>(
                    &row.try_get::<String, _>("project_id").map_err(map_sql)?,
                )?;
                let intent = decode_upload_commit_payload(
                    operation_id,
                    row.try_get("payload_version").map_err(map_sql)?,
                    &row.try_get::<String, _>("payload").map_err(map_sql)?,
                )?;
                if intent.project_id != project_id {
                    return Err(UploadFinalizeRepositoryError::Unavailable);
                }
                Ok(intent)
            })
            .collect()
    }

    async fn fail_upload_commit(
        &self,
        intent: &UploadCommitIntent,
        error_code: &'static str,
        now: OffsetDateTime,
    ) -> Result<(), UploadFinalizeRepositoryError> {
        if error_code.is_empty()
            || error_code.len() > 128
            || !error_code
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b == b'_')
        {
            return Err(UploadFinalizeRepositoryError::Unavailable);
        }
        let mut transaction = self.pool.begin().await.map_err(map_sql)?;
        validate_intent_row(&mut transaction, intent).await?;
        let operation = sqlx::query(
            "UPDATE operation SET state = 'failed', error = ?, updated_at = ?
             WHERE id = ? AND kind = 'upload_finalize' AND state IN ('pending', 'fs_applied')",
        )
        .bind(error_code)
        .bind(timestamp(now)?)
        .bind(intent.operation_id.to_string())
        .execute(&mut *transaction)
        .await
        .map_err(map_sql)?
        .rows_affected();
        let upload = sqlx::query(
            "UPDATE upload_session SET state = 'failed'
             WHERE id = ? AND state = 'committing'",
        )
        .bind(intent.upload_id.to_string())
        .execute(&mut *transaction)
        .await
        .map_err(map_sql)?
        .rows_affected();
        if operation != 1 || upload != 1 {
            return Err(UploadFinalizeRepositoryError::Unavailable);
        }
        transaction.commit().await.map_err(map_sql)
    }
}

fn encode_upload_commit_payload(
    intent: &UploadCommitIntent,
) -> Result<String, UploadFinalizeRepositoryError> {
    validate_components(&intent.destination_components)?;
    FileExactName::parse(intent.destination_name.clone())
        .map_err(|_| UploadFinalizeRepositoryError::Unavailable)?;
    if !is_safe_upload_name(&intent.destination_name) {
        return Err(UploadFinalizeRepositoryError::Unavailable);
    }
    let payload = UploadCommitPayload {
        upload_id: intent.upload_id.to_string(),
        project_id: intent.project_id.to_string(),
        destination_parent_id: intent.destination_parent_id.map(|id| id.to_string()),
        destination_parent_revision: intent
            .destination_parent_revision
            .map(|value| value.to_string()),
        destination_parent_identity: intent
            .destination_parent_identity
            .map(|identity| hex(&identity.as_bytes())),
        destination_components: intent.destination_components.clone(),
        destination_name: intent.destination_name.clone(),
        file_entry_id: intent.file_entry_id.to_string(),
        expected_size: intent.expected_size.to_string(),
        sha256: hex(&intent.sha256),
        staging_identity: hex(&intent.staging_identity.as_bytes()),
        result_identity: intent
            .result_identity
            .map(|identity| hex(&identity.as_bytes())),
    };
    let encoded =
        serde_json::to_string(&payload).map_err(|_| UploadFinalizeRepositoryError::Unavailable)?;
    if encoded.len() > MAX_UPLOAD_COMMIT_PAYLOAD_BYTES {
        return Err(UploadFinalizeRepositoryError::Unavailable);
    }
    Ok(encoded)
}

pub fn decode_upload_commit_payload(
    operation_id: OperationId,
    payload_version: i64,
    payload: &str,
) -> Result<UploadCommitIntent, UploadFinalizeRepositoryError> {
    if payload_version != UPLOAD_COMMIT_PAYLOAD_VERSION
        || payload.len() > MAX_UPLOAD_COMMIT_PAYLOAD_BYTES
    {
        return Err(UploadFinalizeRepositoryError::Unavailable);
    }
    let value: Value =
        serde_json::from_str(payload).map_err(|_| UploadFinalizeRepositoryError::Unavailable)?;
    let payload: UploadCommitPayload =
        serde_json::from_value(value).map_err(|_| UploadFinalizeRepositoryError::Unavailable)?;
    validate_components(&payload.destination_components)?;
    FileExactName::parse(payload.destination_name.clone())
        .map_err(|_| UploadFinalizeRepositoryError::Unavailable)?;
    if !is_safe_upload_name(&payload.destination_name) {
        return Err(UploadFinalizeRepositoryError::Unavailable);
    }
    let expected_size = canonical_i64(&payload.expected_size)?;
    if expected_size < 0 {
        return Err(UploadFinalizeRepositoryError::Unavailable);
    }
    let destination_parent_revision = payload
        .destination_parent_revision
        .as_deref()
        .map(canonical_i64)
        .transpose()?;
    if destination_parent_revision.is_some_and(|value| value < 1)
        || destination_parent_revision.is_some() != payload.destination_parent_identity.is_some()
        || destination_parent_revision.is_some() != payload.destination_parent_id.is_some()
    {
        return Err(UploadFinalizeRepositoryError::Unavailable);
    }
    Ok(UploadCommitIntent {
        operation_id,
        upload_id: canonical_id::<UploadId>(&payload.upload_id)?,
        project_id: canonical_id::<ProjectId>(&payload.project_id)?,
        destination_parent_id: payload
            .destination_parent_id
            .as_deref()
            .map(canonical_id::<FileEntryId>)
            .transpose()?,
        destination_parent_revision,
        destination_parent_identity: payload
            .destination_parent_identity
            .as_deref()
            .map(decode_hex::<24>)
            .transpose()?
            .map(StagingIdentity::new),
        destination_components: payload.destination_components,
        destination_name: payload.destination_name,
        file_entry_id: canonical_id::<FileEntryId>(&payload.file_entry_id)?,
        expected_size,
        sha256: decode_hex::<32>(&payload.sha256)?,
        staging_identity: StagingIdentity::new(decode_hex::<24>(&payload.staging_identity)?),
        result_identity: payload
            .result_identity
            .as_deref()
            .map(decode_hex::<24>)
            .transpose()?
            .map(StagingIdentity::new),
    })
}

async fn validate_active_project(
    transaction: &mut Transaction<'_, Sqlite>,
    project_id: ProjectId,
) -> Result<(), UploadFinalizeRepositoryError> {
    let row = sqlx::query("SELECT status, deleted_at FROM project WHERE id = ?")
        .bind(project_id.to_string())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(map_sql)?
        .ok_or(UploadFinalizeRepositoryError::NotFound)?;
    let status: String = row.try_get("status").map_err(map_sql)?;
    let deleted_at: Option<String> = row.try_get("deleted_at").map_err(map_sql)?;
    if deleted_at.is_some() {
        return Err(UploadFinalizeRepositoryError::NotFound);
    }
    match status.as_str() {
        "active" => Ok(()),
        "archived" => Err(UploadFinalizeRepositoryError::Conflict),
        _ => Err(UploadFinalizeRepositoryError::Unavailable),
    }
}

struct ParentFacts {
    components: Vec<String>,
    revision: Option<i64>,
    identity: Option<StagingIdentity>,
}

async fn parent_facts(
    transaction: &mut Transaction<'_, Sqlite>,
    project_id: ProjectId,
    parent_id: Option<FileEntryId>,
) -> Result<ParentFacts, UploadFinalizeRepositoryError> {
    let mut current = parent_id;
    let mut components = Vec::new();
    let mut visited = HashSet::new();
    let mut revision = None;
    let mut identity = None;
    while let Some(id) = current {
        if components.len() >= MAX_DIRECTORY_DEPTH || !visited.insert(id) {
            return Err(UploadFinalizeRepositoryError::Unavailable);
        }
        let row = sqlx::query(
            "SELECT parent_id, exact_name, kind, state, revision, platform_kind,
                    volume_serial, filesystem_file_id FROM file_entry
             WHERE id = ? AND project_id = ?",
        )
        .bind(id.to_string())
        .bind(project_id.to_string())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(map_sql)?
        .ok_or(UploadFinalizeRepositoryError::NotFound)?;
        let kind: String = row.try_get("kind").map_err(map_sql)?;
        let state: String = row.try_get("state").map_err(map_sql)?;
        if kind != "directory" || !matches!(state.as_str(), "live" | "settling") {
            return Err(UploadFinalizeRepositoryError::Conflict);
        }
        if components.is_empty() {
            let current_revision: i64 = row.try_get("revision").map_err(map_sql)?;
            let platform_kind: String = row.try_get("platform_kind").map_err(map_sql)?;
            let volume = row
                .try_get::<Option<Vec<u8>>, _>("volume_serial")
                .map_err(map_sql)?;
            let file_id = row
                .try_get::<Option<Vec<u8>>, _>("filesystem_file_id")
                .map_err(map_sql)?;
            let parent_identity = match (platform_kind.as_str(), volume, file_id) {
                ("windows_file_id", Some(volume), Some(file_id)) => {
                    let volume: [u8; 8] = volume
                        .try_into()
                        .map_err(|_| UploadFinalizeRepositoryError::Unavailable)?;
                    let file_id: [u8; 16] = file_id
                        .try_into()
                        .map_err(|_| UploadFinalizeRepositoryError::Unavailable)?;
                    let mut bytes = [0_u8; 24];
                    bytes[..8].copy_from_slice(&volume);
                    bytes[8..].copy_from_slice(&file_id);
                    StagingIdentity::new(bytes)
                }
                _ => return Err(UploadFinalizeRepositoryError::Conflict),
            };
            if current_revision < 1 {
                return Err(UploadFinalizeRepositoryError::Unavailable);
            }
            revision = Some(current_revision);
            identity = Some(parent_identity);
        }
        let name = row.try_get::<String, _>("exact_name").map_err(map_sql)?;
        FileExactName::parse(name.clone())
            .map_err(|_| UploadFinalizeRepositoryError::Unavailable)?;
        components.push(name);
        current = row
            .try_get::<Option<String>, _>("parent_id")
            .map_err(map_sql)?
            .map(|value| canonical_id::<FileEntryId>(&value))
            .transpose()?;
    }
    components.reverse();
    validate_components(&components)?;
    Ok(ParentFacts {
        components,
        revision,
        identity,
    })
}

async fn ensure_destination_absent(
    transaction: &mut Transaction<'_, Sqlite>,
    project_id: ProjectId,
    parent_id: Option<FileEntryId>,
    name: &str,
) -> Result<(), UploadFinalizeRepositoryError> {
    let count: i64 = match parent_id {
        Some(parent_id) => {
            sqlx::query_scalar(
                "SELECT count(*) FROM file_entry
             WHERE project_id = ? AND parent_id = ?
               AND exact_name = ? COLLATE WINDOWS_ORDINAL_CI_V1
               AND state IN ('live', 'settling', 'unsupported')",
            )
            .bind(project_id.to_string())
            .bind(parent_id.to_string())
            .bind(name)
            .fetch_one(&mut **transaction)
            .await
        }
        None => {
            sqlx::query_scalar(
                "SELECT count(*) FROM file_entry
             WHERE project_id = ? AND parent_id IS NULL
               AND exact_name = ? COLLATE WINDOWS_ORDINAL_CI_V1
               AND state IN ('live', 'settling', 'unsupported')",
            )
            .bind(project_id.to_string())
            .bind(name)
            .fetch_one(&mut **transaction)
            .await
        }
    }
    .map_err(map_sql)?;
    if count == 0 {
        Ok(())
    } else {
        Err(UploadFinalizeRepositoryError::Conflict)
    }
}

async fn validate_intent_row(
    transaction: &mut Transaction<'_, Sqlite>,
    intent: &UploadCommitIntent,
) -> Result<(), UploadFinalizeRepositoryError> {
    let row = sqlx::query(
        "SELECT o.project_id, o.payload_version, o.payload, o.state,
                f.upload_id, f.file_entry_id
         FROM operation AS o JOIN upload_finalization AS f ON f.operation_id = o.id
         WHERE o.id = ? AND o.kind = 'upload_finalize'",
    )
    .bind(intent.operation_id.to_string())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sql)?
    .ok_or(UploadFinalizeRepositoryError::Unavailable)?;
    let decoded = decode_upload_commit_payload(
        intent.operation_id,
        row.try_get("payload_version").map_err(map_sql)?,
        &row.try_get::<String, _>("payload").map_err(map_sql)?,
    )?;
    let state: String = row.try_get("state").map_err(map_sql)?;
    if decoded != *intent
        || !matches!(state.as_str(), "pending" | "fs_applied")
        || row.try_get::<String, _>("project_id").map_err(map_sql)? != intent.project_id.to_string()
        || row.try_get::<String, _>("upload_id").map_err(map_sql)? != intent.upload_id.to_string()
        || row.try_get::<String, _>("file_entry_id").map_err(map_sql)?
            != intent.file_entry_id.to_string()
    {
        return Err(UploadFinalizeRepositoryError::Unavailable);
    }
    Ok(())
}

async fn read_file_entry(
    pool: &SqlitePool,
    id: FileEntryId,
) -> Result<FileEntry, UploadFinalizeRepositoryError> {
    let row = sqlx::query("SELECT * FROM file_entry WHERE id = ?")
        .bind(id.to_string())
        .fetch_optional(pool)
        .await
        .map_err(map_sql)?
        .ok_or(UploadFinalizeRepositoryError::Unavailable)?;
    row_to_file_entry(row)
}

fn row_to_file_entry(row: SqliteRow) -> Result<FileEntry, UploadFinalizeRepositoryError> {
    let id = canonical_id::<FileEntryId>(&row.try_get::<String, _>("id").map_err(map_sql)?)?;
    let project_id =
        canonical_id::<ProjectId>(&row.try_get::<String, _>("project_id").map_err(map_sql)?)?;
    let parent_id = row
        .try_get::<Option<String>, _>("parent_id")
        .map_err(map_sql)?
        .map(|value| canonical_id::<FileEntryId>(&value))
        .transpose()?;
    let exact_name = FileExactName::parse(row.try_get::<String, _>("exact_name").map_err(map_sql)?)
        .map_err(|_| UploadFinalizeRepositoryError::Unavailable)?;
    let platform_identity = PlatformIdentity::try_new(
        row.try_get::<String, _>("platform_kind").map_err(map_sql)?,
        row.try_get::<Option<Vec<u8>>, _>("volume_serial")
            .map_err(map_sql)?,
        row.try_get::<Option<Vec<u8>>, _>("filesystem_file_id")
            .map_err(map_sql)?,
    )
    .map_err(|_| UploadFinalizeRepositoryError::Unavailable)?;
    let hash = digest(row.try_get::<Option<Vec<u8>>, _>("hash").map_err(map_sql)?)?;
    let hash_state = row
        .try_get::<String, _>("hash_state")
        .map_err(map_sql)?
        .parse::<FileHashState>()
        .map_err(|_| UploadFinalizeRepositoryError::Unavailable)?;
    if hash_state != FileHashState::Ready || hash.is_none() {
        return Err(UploadFinalizeRepositoryError::Unavailable);
    }
    let size: i64 = row.try_get("size").map_err(map_sql)?;
    let mtime: i64 = row.try_get("mtime_filetime_100ns").map_err(map_sql)?;
    let revision: i64 = row.try_get("revision").map_err(map_sql)?;
    let scan_generation: i64 = row.try_get("scan_generation").map_err(map_sql)?;
    if size < 0 || mtime < 0 || revision < 1 || scan_generation < 0 {
        return Err(UploadFinalizeRepositoryError::Unavailable);
    }
    Ok(FileEntry {
        id,
        project_id,
        parent_id,
        relative_path: exact_name.as_str().to_owned(),
        exact_name,
        kind: row
            .try_get::<String, _>("kind")
            .map_err(map_sql)?
            .parse::<FileKind>()
            .map_err(|_| UploadFinalizeRepositoryError::Unavailable)?,
        platform_identity,
        size,
        mtime_filetime_100ns: mtime,
        hash,
        hash_state,
        state: row
            .try_get::<String, _>("state")
            .map_err(map_sql)?
            .parse::<FileState>()
            .map_err(|_| UploadFinalizeRepositoryError::Unavailable)?,
        revision,
        scan_generation,
        observed_at: parse_timestamp(&row.try_get::<String, _>("observed_at").map_err(map_sql)?)?,
    })
}

fn validate_components(components: &[String]) -> Result<(), UploadFinalizeRepositoryError> {
    if components.len() > MAX_UPLOAD_COMMIT_COMPONENTS {
        return Err(UploadFinalizeRepositoryError::Unavailable);
    }
    for component in components {
        FileExactName::parse(component.clone())
            .map_err(|_| UploadFinalizeRepositoryError::Unavailable)?;
        if !is_safe_upload_name(component) {
            return Err(UploadFinalizeRepositoryError::Unavailable);
        }
    }
    Ok(())
}

fn intent_relative_path(intent: &UploadCommitIntent) -> String {
    intent
        .destination_components
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(intent.destination_name.as_str()))
        .collect::<Vec<_>>()
        .join("/")
}

fn canonical_id<T>(value: &str) -> Result<T, UploadFinalizeRepositoryError>
where
    T: std::str::FromStr + ToString,
{
    let parsed = value
        .parse::<T>()
        .map_err(|_| UploadFinalizeRepositoryError::Unavailable)?;
    if parsed.to_string() != value {
        return Err(UploadFinalizeRepositoryError::Unavailable);
    }
    Ok(parsed)
}

fn canonical_i64(value: &str) -> Result<i64, UploadFinalizeRepositoryError> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return Err(UploadFinalizeRepositoryError::Unavailable);
    }
    value
        .parse()
        .map_err(|_| UploadFinalizeRepositoryError::Unavailable)
}

fn digest(value: Option<Vec<u8>>) -> Result<Option<[u8; 32]>, UploadFinalizeRepositoryError> {
    value
        .map(|value| {
            value
                .try_into()
                .map_err(|_| UploadFinalizeRepositoryError::Unavailable)
        })
        .transpose()
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[(byte >> 4) as usize]));
        output.push(char::from(HEX[(byte & 0x0f) as usize]));
    }
    output
}

fn decode_hex<const N: usize>(value: &str) -> Result<[u8; N], UploadFinalizeRepositoryError> {
    if value.len() != N * 2
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(UploadFinalizeRepositoryError::Unavailable);
    }
    let mut output = [0_u8; N];
    for (index, slot) in output.iter_mut().enumerate() {
        let start = index * 2;
        *slot = u8::from_str_radix(&value[start..start + 2], 16)
            .map_err(|_| UploadFinalizeRepositoryError::Unavailable)?;
    }
    Ok(output)
}

fn timestamp(value: OffsetDateTime) -> Result<String, UploadFinalizeRepositoryError> {
    let format = time::format_description::parse_borrowed::<2>(FIXED_UTC_TIMESTAMP_FORMAT)
        .map_err(|_| UploadFinalizeRepositoryError::Unavailable)?;
    value
        .to_offset(UtcOffset::UTC)
        .format(&format)
        .map_err(|_| UploadFinalizeRepositoryError::Unavailable)
}

fn parse_timestamp(value: &str) -> Result<OffsetDateTime, UploadFinalizeRepositoryError> {
    let format = time::format_description::parse_borrowed::<2>(FIXED_UTC_TIMESTAMP_FORMAT)
        .map_err(|_| UploadFinalizeRepositoryError::Unavailable)?;
    PrimitiveDateTime::parse(value, &format)
        .map(PrimitiveDateTime::assume_utc)
        .map_err(|_| UploadFinalizeRepositoryError::Unavailable)
}

fn map_sql(_: sqlx::Error) -> UploadFinalizeRepositoryError {
    UploadFinalizeRepositoryError::Unavailable
}

fn map_constraint(error: sqlx::Error) -> UploadFinalizeRepositoryError {
    if matches!(&error, sqlx::Error::Database(database) if database.is_unique_violation() || database.is_foreign_key_violation())
    {
        UploadFinalizeRepositoryError::Conflict
    } else {
        UploadFinalizeRepositoryError::Unavailable
    }
}
