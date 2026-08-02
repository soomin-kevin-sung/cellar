#[cfg(windows)]
mod platform {
    use std::collections::HashSet;
    use std::str::FromStr;
    use std::sync::Arc;

    use async_trait::async_trait;
    use cellar_api::routes::files::{FileMutationCommand, FileMutationError, FileMutationSource};
    use cellar_core::{
        FileEntry, FileEntryId, FileExactName, FileHashState, FileKind, FileState,
        InMemoryProjectMutationCoordinator, OperationId, PlatformIdentity, ProjectId,
        ProjectMutationCoordinator,
    };
    use cellar_windows::{
        StorageError, StorageErrorKind, VerifiedHandle, WindowsName, WindowsStorage,
    };
    use serde::{Deserialize, Serialize};
    use sqlx::{Row, Sqlite, SqlitePool, Transaction};
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    use crate::downloads::{ReconciliationRequest, ReconciliationScheduler};

    const PAYLOAD_VERSION: i64 = 1;
    const STAGING_DIRECTORY: &str = ".cellar-file-mutation-staging";
    const MAX_DIRECTORY_DEPTH: usize = 256;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum FileMutationFaultPoint {
        AfterIntent,
        AfterTemporaryRename,
        AfterStagingIntent,
        CopyIoFailure,
        AfterCopyStaging,
        AfterFilesystemApply,
    }

    pub trait FileMutationFaultInjector: Send + Sync {
        fn should_fail(&self, point: FileMutationFaultPoint) -> bool;
    }

    struct NoFaults;

    impl FileMutationFaultInjector for NoFaults {
        fn should_fail(&self, _: FileMutationFaultPoint) -> bool {
            false
        }
    }

    struct NoReconciliation;

    impl ReconciliationScheduler for NoReconciliation {
        fn try_schedule(&self, _: ReconciliationRequest) -> bool {
            true
        }
    }

    #[derive(Clone)]
    pub struct ProductionFileMutationSource {
        pool: SqlitePool,
        storage: WindowsStorage,
        staging: VerifiedHandle,
        project_mutations: Arc<dyn ProjectMutationCoordinator>,
        faults: Arc<dyn FileMutationFaultInjector>,
        reconciliation: Arc<dyn ReconciliationScheduler>,
    }

    #[derive(Clone, Debug)]
    struct CatalogEntry {
        id: FileEntryId,
        project_id: ProjectId,
        parent_id: Option<FileEntryId>,
        exact_name: String,
        kind: FileKind,
        identity: [u8; 24],
        size: i64,
        mtime: i64,
        hash: Option<[u8; 32]>,
        hash_state: FileHashState,
        state: FileState,
        revision: i64,
        scan_generation: i64,
        observed_at: OffsetDateTime,
    }

    #[derive(Clone)]
    struct Namespace {
        components: Vec<String>,
        handle: VerifiedHandle,
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct MutationPayload {
        mutation: MutationKind,
        file_id: String,
        result_file_id: String,
        expected_revision: String,
        source_parent_id: Option<String>,
        source_components: Vec<String>,
        source_name: String,
        destination_parent_id: Option<String>,
        destination_components: Vec<String>,
        destination_name: String,
        source_identity: String,
        source_namespace_identity: String,
        destination_namespace_identity: String,
        expected_size: String,
        expected_mtime: String,
        temporary_name: Option<String>,
        staging_name: Option<String>,
        staging_identity: Option<String>,
        staging_complete: bool,
        result_identity: Option<String>,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum MutationKind {
        Rename,
        Move,
        Copy,
    }

    impl MutationKind {
        fn operation_kind(self) -> &'static str {
            match self {
                Self::Rename => "file_rename",
                Self::Move => "file_move",
                Self::Copy => "file_copy",
            }
        }
    }

    impl ProductionFileMutationSource {
        pub async fn open(
            pool: SqlitePool,
            storage: WindowsStorage,
        ) -> Result<Self, FileMutationError> {
            Self::open_with_dependencies(
                pool,
                storage,
                Arc::new(NoFaults),
                Arc::new(NoReconciliation),
                Arc::new(InMemoryProjectMutationCoordinator::default()),
            )
            .await
        }

        pub async fn open_with_reconciliation(
            pool: SqlitePool,
            storage: WindowsStorage,
            reconciliation: Arc<dyn ReconciliationScheduler>,
            project_mutations: Arc<dyn ProjectMutationCoordinator>,
        ) -> Result<Self, FileMutationError> {
            Self::open_with_dependencies(
                pool,
                storage,
                Arc::new(NoFaults),
                reconciliation,
                project_mutations,
            )
            .await
        }

        pub async fn open_with_fault_injector(
            pool: SqlitePool,
            storage: WindowsStorage,
            faults: Arc<dyn FileMutationFaultInjector>,
        ) -> Result<Self, FileMutationError> {
            Self::open_with_dependencies(
                pool,
                storage,
                faults,
                Arc::new(NoReconciliation),
                Arc::new(InMemoryProjectMutationCoordinator::default()),
            )
            .await
        }

        pub async fn open_with_fault_injector_and_reconciliation(
            pool: SqlitePool,
            storage: WindowsStorage,
            faults: Arc<dyn FileMutationFaultInjector>,
            reconciliation: Arc<dyn ReconciliationScheduler>,
            project_mutations: Arc<dyn ProjectMutationCoordinator>,
        ) -> Result<Self, FileMutationError> {
            Self::open_with_dependencies(pool, storage, faults, reconciliation, project_mutations)
                .await
        }

        async fn open_with_dependencies(
            pool: SqlitePool,
            storage: WindowsStorage,
            faults: Arc<dyn FileMutationFaultInjector>,
            reconciliation: Arc<dyn ReconciliationScheduler>,
            project_mutations: Arc<dyn ProjectMutationCoordinator>,
        ) -> Result<Self, FileMutationError> {
            let name = WindowsName::parse(STAGING_DIRECTORY)
                .map_err(|_| FileMutationError::Unavailable)?;
            let staging = match storage.create_directory_no_replace(storage.root(), &name) {
                Ok(handle) => handle,
                Err(error) if error.kind() == StorageErrorKind::Conflict => storage
                    .open_verified_directory_stable(storage.root(), &name)
                    .map_err(map_storage)?,
                Err(error) => return Err(map_storage(error)),
            };
            Ok(Self {
                pool,
                storage,
                staging,
                project_mutations,
                faults,
                reconciliation,
            })
        }

        pub async fn recover_pending(&self) -> Result<(), FileMutationError> {
            let rows = sqlx::query(
                "SELECT id, project_id, kind, payload_version, payload
                 FROM operation
                 WHERE state IN ('pending', 'fs_applied')
                   AND kind IN ('file_rename', 'file_move', 'file_copy')
                 ORDER BY created_at, id",
            )
            .fetch_all(&self.pool)
            .await
            .map_err(map_sql)?;
            for row in rows {
                let operation_id = canonical::<OperationId>(row.try_get("id").map_err(map_sql)?)?;
                let project_id = canonical::<ProjectId>(
                    row.try_get::<String, _>("project_id").map_err(map_sql)?,
                )?;
                let operation_kind: String = row.try_get("kind").map_err(map_sql)?;
                let version: i64 = row.try_get("payload_version").map_err(map_sql)?;
                let payload_text: String = row.try_get("payload").map_err(map_sql)?;
                if version != PAYLOAD_VERSION {
                    self.fail_operation(operation_id, "unsupported_payload")
                        .await?;
                    continue;
                }
                let payload: MutationPayload = match serde_json::from_str(&payload_text) {
                    Ok(payload) => payload,
                    Err(_) => {
                        self.fail_operation(operation_id, "invalid_payload").await?;
                        continue;
                    }
                };
                if operation_kind != payload.mutation.operation_kind() {
                    self.fail_operation(operation_id, "invalid_payload").await?;
                    continue;
                }
                let _guard = self
                    .project_mutations
                    .project_lock(project_id)
                    .lock_owned()
                    .await;
                if let Err(error) = self.recover_one(operation_id, project_id, &payload).await {
                    if matches!(
                        error,
                        FileMutationError::Conflict | FileMutationError::FileNotFound
                    ) {
                        self.fail_operation(operation_id, error.code()).await?;
                    } else {
                        return Err(error);
                    }
                }
            }
            Ok(())
        }

        async fn execute(
            &self,
            project_id: ProjectId,
            file_id: FileEntryId,
            command: FileMutationCommand,
        ) -> Result<FileEntry, FileMutationError> {
            let _guard = self
                .project_mutations
                .project_lock(project_id)
                .lock_owned()
                .await;
            ensure_no_pending_mutation(&self.pool, project_id).await?;
            validate_project(&self.pool, project_id).await?;
            let source = load_entry(&self.pool, project_id, file_id).await?;
            if source.state != FileState::Live {
                return Err(FileMutationError::Unsupported);
            }
            let (kind, expected_revision, destination_parent_id, destination_name) = match command {
                FileMutationCommand::Rename {
                    expected_revision,
                    name,
                } => (
                    MutationKind::Rename,
                    expected_revision,
                    source.parent_id,
                    name,
                ),
                FileMutationCommand::Move {
                    expected_revision,
                    destination_parent_id,
                } => (
                    MutationKind::Move,
                    expected_revision,
                    destination_parent_id,
                    FileExactName::parse(source.exact_name.clone())
                        .map_err(|_| FileMutationError::Unavailable)?,
                ),
                FileMutationCommand::Copy {
                    expected_revision,
                    destination_parent_id,
                    name,
                } => {
                    if source.kind != FileKind::File {
                        return Err(FileMutationError::Unsupported);
                    }
                    (
                        MutationKind::Copy,
                        expected_revision,
                        destination_parent_id,
                        name,
                    )
                }
            };
            if source.revision != expected_revision {
                return Err(FileMutationError::StaleRevision);
            }
            let destination_name = WindowsName::parse(destination_name.as_str().to_owned())
                .map_err(|_| FileMutationError::InvalidName)?;
            let source_namespace = self.namespace(project_id, source.parent_id).await?;
            let destination_namespace = if source.parent_id == destination_parent_id {
                source_namespace.clone()
            } else {
                self.namespace(project_id, destination_parent_id).await?
            };
            if source.kind == FileKind::Directory
                && kind == MutationKind::Move
                && is_descendant(&self.pool, project_id, destination_parent_id, source.id).await?
            {
                return Err(FileMutationError::Conflict);
            }
            let source_name = WindowsName::parse(source.exact_name.clone())
                .map_err(|_| FileMutationError::Unavailable)?;
            let source_handle = self
                .storage
                .open_verified_for_namespace_mutation(&source_namespace.handle, &source_name)
                .map_err(map_storage)?;
            if identity_bytes(&source_handle) != source.identity {
                return Err(FileMutationError::Conflict);
            }
            if kind == MutationKind::Copy {
                let (size, mtime) = self
                    .storage
                    .file_length_and_mtime(&source_handle)
                    .map_err(map_storage)?;
                if size != source.size || mtime != source.mtime {
                    return Err(FileMutationError::Conflict);
                }
            }
            let same_parent = source.parent_id == destination_parent_id;
            let same_spelling = source.exact_name == destination_name.as_str();
            if kind != MutationKind::Copy && same_parent && same_spelling {
                return entry_to_public(source, source_namespace.components);
            }
            let case_only = kind == MutationKind::Rename
                && same_parent
                && names_equal_ci(&self.pool, &source.exact_name, destination_name.as_str())
                    .await?;
            ensure_catalog_destination_absent(
                &self.pool,
                project_id,
                destination_parent_id,
                destination_name.as_str(),
                (kind != MutationKind::Copy).then_some(source.id),
            )
            .await?;
            if !case_only {
                ensure_physical_destination_absent(
                    &self.storage,
                    &destination_namespace.handle,
                    &destination_name,
                    None,
                )?;
            }

            let operation_id = OperationId::new();
            let result_file_id = if kind == MutationKind::Copy {
                FileEntryId::new()
            } else {
                source.id
            };
            let temporary_name = case_only.then(|| format!(".cellar-case-{}", operation_id));
            let staging_name =
                (kind == MutationKind::Copy).then(|| format!("copy-{}.stage", operation_id));
            let mut payload = MutationPayload {
                mutation: kind,
                file_id: source.id.to_string(),
                result_file_id: result_file_id.to_string(),
                expected_revision: expected_revision.to_string(),
                source_parent_id: source.parent_id.map(|id| id.to_string()),
                source_components: source_namespace.components,
                source_name: source.exact_name.clone(),
                destination_parent_id: destination_parent_id.map(|id| id.to_string()),
                destination_components: destination_namespace.components,
                destination_name: destination_name.as_str().to_owned(),
                source_identity: encode_identity(source.identity),
                source_namespace_identity: encode_identity(identity_bytes(
                    &source_namespace.handle,
                )),
                destination_namespace_identity: encode_identity(identity_bytes(
                    &destination_namespace.handle,
                )),
                expected_size: source.size.to_string(),
                expected_mtime: source.mtime.to_string(),
                temporary_name,
                staging_name,
                staging_identity: None,
                staging_complete: false,
                result_identity: None,
            };
            insert_operation(&self.pool, operation_id, project_id, &payload).await?;
            if self.faults.should_fail(FileMutationFaultPoint::AfterIntent) {
                return Err(FileMutationError::Unavailable);
            }

            let result_handle = match kind {
                MutationKind::Rename | MutationKind::Move => {
                    if let Some(temporary) = &payload.temporary_name {
                        let temporary = WindowsName::parse(temporary.clone())
                            .map_err(|_| FileMutationError::Unavailable)?;
                        if let Err(storage_error) = self.storage.rename_no_replace(
                            &source_handle,
                            &source_namespace.handle,
                            &temporary,
                        ) {
                            let error = map_storage(storage_error);
                            self.fail_operation(operation_id, error.code()).await?;
                            return Err(error);
                        }
                        if self
                            .faults
                            .should_fail(FileMutationFaultPoint::AfterTemporaryRename)
                        {
                            return Err(FileMutationError::Unavailable);
                        }
                        if identity_bytes(&source_handle) != source.identity {
                            self.fail_operation(operation_id, FileMutationError::Conflict.code())
                                .await?;
                            return Err(FileMutationError::Conflict);
                        }
                    }
                    if let Err(storage_error) = self.storage.rename_no_replace(
                        &source_handle,
                        &destination_namespace.handle,
                        &destination_name,
                    ) {
                        let error = map_storage(storage_error);
                        if payload.temporary_name.is_some()
                            && self
                                .storage
                                .rename_no_replace(
                                    &source_handle,
                                    &source_namespace.handle,
                                    &source_name,
                                )
                                .is_err()
                        {
                            self.fail_operation(operation_id, "file_mutation_rollback_failed")
                                .await?;
                            return Err(error);
                        }
                        self.fail_operation(operation_id, error.code()).await?;
                        return Err(error);
                    }
                    source_handle
                }
                MutationKind::Copy => {
                    let staging_name = WindowsName::parse(
                        payload
                            .staging_name
                            .clone()
                            .ok_or(FileMutationError::Unavailable)?,
                    )
                    .map_err(|_| FileMutationError::Unavailable)?;
                    let staging = match self
                        .storage
                        .create_staging_file_no_replace(&self.staging, &staging_name)
                    {
                        Ok(handle) => handle,
                        Err(storage_error) => {
                            let error = map_storage(storage_error);
                            self.fail_operation(operation_id, error.code()).await?;
                            return Err(error);
                        }
                    };
                    payload.staging_identity = Some(encode_identity(identity_bytes(&staging)));
                    update_pending_payload(&self.pool, operation_id, &payload).await?;
                    if self
                        .faults
                        .should_fail(FileMutationFaultPoint::AfterStagingIntent)
                    {
                        return Err(FileMutationError::Unavailable);
                    }
                    let copy_result = if self
                        .faults
                        .should_fail(FileMutationFaultPoint::CopyIoFailure)
                    {
                        Err(FileMutationError::InsufficientStorage)
                    } else {
                        copy_file_and_flush_blocking(&self.storage, &source_handle, &staging).await
                    };
                    let copied = match copy_result {
                        Ok(copied) => copied,
                        Err(error) => {
                            return Err(self
                                .terminalize_failed_copy(operation_id, staging, error)
                                .await?);
                        }
                    };
                    let staging_length = match self.storage.file_length(&staging) {
                        Ok(length) => length,
                        Err(storage_error) => {
                            return Err(self
                                .terminalize_failed_copy(
                                    operation_id,
                                    staging,
                                    map_storage(storage_error),
                                )
                                .await?);
                        }
                    };
                    if copied != source.size || staging_length != source.size {
                        return Err(self
                            .terminalize_failed_copy(
                                operation_id,
                                staging,
                                FileMutationError::Unavailable,
                            )
                            .await?);
                    }
                    payload.staging_complete = true;
                    update_pending_payload(&self.pool, operation_id, &payload).await?;
                    if self
                        .faults
                        .should_fail(FileMutationFaultPoint::AfterCopyStaging)
                    {
                        return Err(FileMutationError::Unavailable);
                    }
                    if let Err(storage_error) = self.storage.rename_no_replace(
                        &staging,
                        &destination_namespace.handle,
                        &destination_name,
                    ) {
                        let error = map_storage(storage_error);
                        self.fail_operation(operation_id, error.code()).await?;
                        return Err(error);
                    }
                    staging
                }
            };
            payload.result_identity = Some(encode_identity(identity_bytes(&result_handle)));
            drop(result_handle);
            if self
                .faults
                .should_fail(FileMutationFaultPoint::AfterFilesystemApply)
            {
                return Err(FileMutationError::Unavailable);
            }
            mark_fs_applied(&self.pool, operation_id, &payload).await?;
            self.complete(operation_id, project_id, &payload).await
        }

        async fn namespace(
            &self,
            project_id: ProjectId,
            parent_id: Option<FileEntryId>,
        ) -> Result<Namespace, FileMutationError> {
            let relative = directory_components(&self.pool, project_id, parent_id).await?;
            let namespace = self.namespace_from_components(project_id, &relative)?;
            if let Some(parent_id) = parent_id {
                let parent = load_entry(&self.pool, project_id, parent_id).await?;
                if parent.kind != FileKind::Directory
                    || parent.state != FileState::Live
                    || identity_bytes(&namespace.handle) != parent.identity
                {
                    return Err(FileMutationError::Conflict);
                }
            }
            Ok(namespace)
        }

        fn namespace_from_components(
            &self,
            project_id: ProjectId,
            relative: &[String],
        ) -> Result<Namespace, FileMutationError> {
            let mut components = vec![
                "projects".to_owned(),
                project_id.to_string(),
                "files".to_owned(),
            ];
            components.extend(relative.iter().cloned());
            let mut handle = self.storage.root().clone();
            for component in &components {
                let name = WindowsName::parse(component.clone())
                    .map_err(|_| FileMutationError::Unavailable)?;
                handle = self
                    .storage
                    .open_verified_directory_stable(&handle, &name)
                    .map_err(map_storage)?;
            }
            Ok(Namespace {
                components: relative.to_vec(),
                handle,
            })
        }

        async fn complete(
            &self,
            operation_id: OperationId,
            project_id: ProjectId,
            payload: &MutationPayload,
        ) -> Result<FileEntry, FileMutationError> {
            let file_id = canonical::<FileEntryId>(payload.file_id.clone())?;
            let result_id = canonical::<FileEntryId>(payload.result_file_id.clone())?;
            let expected_revision = payload
                .expected_revision
                .parse::<i64>()
                .map_err(|_| FileMutationError::Unavailable)?;
            let destination_parent_id = payload
                .destination_parent_id
                .clone()
                .map(canonical::<FileEntryId>)
                .transpose()?;
            let identity = decode_identity(
                payload
                    .result_identity
                    .as_deref()
                    .unwrap_or(&payload.source_identity),
            )?;
            let destination_namespace =
                self.namespace_from_components(project_id, &payload.destination_components)?;
            if identity_bytes(&destination_namespace.handle)
                != decode_identity(&payload.destination_namespace_identity)?
            {
                return Err(FileMutationError::Conflict);
            }
            let result_handle = self
                .storage
                .open_verified(
                    &destination_namespace.handle,
                    &WindowsName::parse(payload.destination_name.clone())
                        .map_err(|_| FileMutationError::Unavailable)?,
                )
                .map_err(map_storage)?;
            if identity_bytes(&result_handle) != identity {
                return Err(FileMutationError::Conflict);
            }
            let (size, mtime) = if payload.mutation == MutationKind::Copy {
                self.storage
                    .file_length_and_mtime(&result_handle)
                    .map_err(map_storage)?
            } else {
                let source = load_entry(&self.pool, project_id, file_id).await?;
                (source.size, source.mtime)
            };
            let now = timestamp()?;
            let mut transaction = self.pool.begin().await.map_err(map_sql)?;
            match payload.mutation {
                MutationKind::Rename | MutationKind::Move => {
                    let affected = sqlx::query(
                        "UPDATE file_entry
                         SET parent_id = ?, exact_name = ?, revision = revision + 1,
                             observed_at = ?
                         WHERE id = ? AND project_id = ? AND revision = ? AND state = 'live'",
                    )
                    .bind(destination_parent_id.map(|id| id.to_string()))
                    .bind(&payload.destination_name)
                    .bind(&now)
                    .bind(file_id.to_string())
                    .bind(project_id.to_string())
                    .bind(expected_revision)
                    .execute(&mut *transaction)
                    .await
                    .map_err(map_sql)?
                    .rows_affected();
                    if affected != 1 {
                        return Err(FileMutationError::StaleRevision);
                    }
                }
                MutationKind::Copy => {
                    if !payload.staging_complete
                        || payload.staging_identity.as_deref() != payload.result_identity.as_deref()
                    {
                        return Err(FileMutationError::Conflict);
                    }
                    let source = load_entry_tx(&mut transaction, project_id, file_id).await?;
                    if source.revision != expected_revision
                        || source.identity != decode_identity(&payload.source_identity)?
                    {
                        return Err(FileMutationError::StaleRevision);
                    }
                    insert_copy(
                        &mut transaction,
                        &source,
                        result_id,
                        destination_parent_id,
                        &payload.destination_name,
                        identity,
                        size,
                        mtime,
                        &now,
                    )
                    .await?;
                }
            }
            let payload_text =
                serde_json::to_string(payload).map_err(|_| FileMutationError::Unavailable)?;
            let affected = sqlx::query(
                "UPDATE operation SET state = 'complete', payload = ?, error = NULL, updated_at = ?
                 WHERE id = ? AND project_id = ? AND state IN ('pending', 'fs_applied')",
            )
            .bind(payload_text)
            .bind(&now)
            .bind(operation_id.to_string())
            .bind(project_id.to_string())
            .execute(&mut *transaction)
            .await
            .map_err(map_sql)?
            .rows_affected();
            if affected != 1 {
                return Err(FileMutationError::Unavailable);
            }
            transaction.commit().await.map_err(map_sql)?;
            let entry = load_entry(&self.pool, project_id, result_id).await?;
            entry_to_public(entry, payload.destination_components.clone())
        }

        async fn recover_one(
            &self,
            operation_id: OperationId,
            project_id: ProjectId,
            payload: &MutationPayload,
        ) -> Result<(), FileMutationError> {
            let expected = decode_identity(&payload.source_identity)?;
            let source_namespace =
                self.namespace_from_components(project_id, &payload.source_components)?;
            let destination_namespace =
                if payload.source_components == payload.destination_components {
                    source_namespace.clone()
                } else {
                    self.namespace_from_components(project_id, &payload.destination_components)?
                };
            if identity_bytes(&source_namespace.handle)
                != decode_identity(&payload.source_namespace_identity)?
                || identity_bytes(&destination_namespace.handle)
                    != decode_identity(&payload.destination_namespace_identity)?
            {
                return Err(FileMutationError::Conflict);
            }
            let source_name = WindowsName::parse(payload.source_name.clone())
                .map_err(|_| FileMutationError::Unavailable)?;
            let destination_name = WindowsName::parse(payload.destination_name.clone())
                .map_err(|_| FileMutationError::Unavailable)?;
            match payload.mutation {
                MutationKind::Rename | MutationKind::Move => {
                    let temporary = payload
                        .temporary_name
                        .as_ref()
                        .map(|name| WindowsName::parse(name.clone()))
                        .transpose()
                        .map_err(|_| FileMutationError::Unavailable)?;
                    let mut candidate = None;
                    let mut candidate_is_temporary = false;
                    if let Some(name) = &temporary
                        && let Some(handle) = open_for_mutation_if_present(
                            &self.storage,
                            &source_namespace.handle,
                            name,
                        )?
                    {
                        if identity_bytes(&handle) != expected {
                            return Err(FileMutationError::Conflict);
                        }
                        candidate = Some(handle);
                        candidate_is_temporary = true;
                    }
                    if candidate_is_temporary
                        && open_for_mutation_if_present(
                            &self.storage,
                            &destination_namespace.handle,
                            &destination_name,
                        )?
                        .is_some()
                    {
                        return Err(FileMutationError::Conflict);
                    }
                    if candidate.is_none()
                        && let Some(handle) = open_for_mutation_if_present(
                            &self.storage,
                            &destination_namespace.handle,
                            &destination_name,
                        )?
                    {
                        if identity_bytes(&handle) != expected {
                            return Err(FileMutationError::Conflict);
                        }
                        let exact = self.storage.current_name(&handle).map_err(map_storage)?;
                        if exact == payload.destination_name {
                            if temporary.is_none()
                                && open_for_mutation_if_present(
                                    &self.storage,
                                    &source_namespace.handle,
                                    &source_name,
                                )?
                                .is_some()
                            {
                                return Err(FileMutationError::Conflict);
                            }
                            let mut completed = payload.clone();
                            completed.result_identity = Some(payload.source_identity.clone());
                            drop(handle);
                            mark_fs_applied(&self.pool, operation_id, &completed).await?;
                            self.complete(operation_id, project_id, &completed).await?;
                            return Ok(());
                        }
                        candidate = Some(handle);
                    }
                    if candidate.is_none() {
                        candidate = open_for_mutation_if_present(
                            &self.storage,
                            &source_namespace.handle,
                            &source_name,
                        )?;
                    }
                    let mut handle = candidate.ok_or(FileMutationError::FileNotFound)?;
                    if identity_bytes(&handle) != expected {
                        return Err(FileMutationError::Conflict);
                    }
                    if let Some(temporary) = temporary.as_ref()
                        && self.storage.current_name(&handle).map_err(map_storage)?
                            == payload.source_name
                    {
                        self.storage
                            .rename_no_replace(&handle, &source_namespace.handle, temporary)
                            .map_err(map_storage)?;
                        drop(handle);
                        handle = open_for_mutation_if_present(
                            &self.storage,
                            &source_namespace.handle,
                            temporary,
                        )?
                        .ok_or(FileMutationError::FileNotFound)?;
                        if identity_bytes(&handle) != expected {
                            return Err(FileMutationError::Conflict);
                        }
                    }
                    self.storage
                        .rename_no_replace(
                            &handle,
                            &destination_namespace.handle,
                            &destination_name,
                        )
                        .map_err(map_storage)?;
                    drop(handle);
                    let mut completed = payload.clone();
                    completed.result_identity = Some(payload.source_identity.clone());
                    mark_fs_applied(&self.pool, operation_id, &completed).await?;
                    self.complete(operation_id, project_id, &completed).await?;
                }
                MutationKind::Copy => {
                    let expected_size = payload
                        .expected_size
                        .parse::<i64>()
                        .map_err(|_| FileMutationError::Unavailable)?;
                    let expected_mtime = payload
                        .expected_mtime
                        .parse::<i64>()
                        .map_err(|_| FileMutationError::Unavailable)?;
                    if expected_size < 0 || expected_mtime < 0 {
                        return Err(FileMutationError::Unavailable);
                    }
                    let expected_staging = payload
                        .staging_identity
                        .as_deref()
                        .map(decode_identity)
                        .transpose()?;
                    if let Some(destination) = open_for_mutation_if_present(
                        &self.storage,
                        &destination_namespace.handle,
                        &destination_name,
                    )? {
                        if !payload.staging_complete
                            || expected_staging != Some(identity_bytes(&destination))
                            || self
                                .storage
                                .file_length(&destination)
                                .map_err(map_storage)?
                                != expected_size
                        {
                            return Err(FileMutationError::Conflict);
                        }
                        let mut completed = payload.clone();
                        completed.result_identity =
                            Some(encode_identity(identity_bytes(&destination)));
                        drop(destination);
                        mark_fs_applied(&self.pool, operation_id, &completed).await?;
                        self.complete(operation_id, project_id, &completed).await?;
                        return Ok(());
                    }
                    let source = open_for_mutation_if_present(
                        &self.storage,
                        &source_namespace.handle,
                        &source_name,
                    )?
                    .ok_or(FileMutationError::FileNotFound)?;
                    if identity_bytes(&source) != expected {
                        return Err(FileMutationError::Conflict);
                    }
                    let (source_size, source_mtime) = self
                        .storage
                        .file_length_and_mtime(&source)
                        .map_err(map_storage)?;
                    if source_size != expected_size || source_mtime != expected_mtime {
                        return Err(FileMutationError::Conflict);
                    }
                    let staging_name = WindowsName::parse(
                        payload
                            .staging_name
                            .clone()
                            .ok_or(FileMutationError::Unavailable)?,
                    )
                    .map_err(|_| FileMutationError::Unavailable)?;
                    let mut completed = payload.clone();
                    let existing_staging = if payload.staging_complete {
                        open_for_mutation_if_present(&self.storage, &self.staging, &staging_name)?
                    } else {
                        open_writable_if_present(&self.storage, &self.staging, &staging_name)?
                    };
                    let staging = match existing_staging {
                        Some(handle) => {
                            if expected_staging != Some(identity_bytes(&handle)) {
                                return Err(FileMutationError::Conflict);
                            }
                            if payload.staging_complete {
                                if self.storage.file_length(&handle).map_err(map_storage)?
                                    != expected_size
                                {
                                    return Err(FileMutationError::Conflict);
                                }
                                handle
                            } else {
                                let copied =
                                    copy_file_and_flush_blocking(&self.storage, &source, &handle)
                                        .await?;
                                if copied != expected_size
                                    || self.storage.file_length(&handle).map_err(map_storage)?
                                        != expected_size
                                {
                                    return Err(FileMutationError::Unavailable);
                                }
                                completed.staging_complete = true;
                                update_pending_payload(&self.pool, operation_id, &completed)
                                    .await?;
                                handle
                            }
                        }
                        None => {
                            let handle = self
                                .storage
                                .create_staging_file_no_replace(&self.staging, &staging_name)
                                .map_err(map_storage)?;
                            completed.staging_identity =
                                Some(encode_identity(identity_bytes(&handle)));
                            completed.staging_complete = false;
                            update_pending_payload(&self.pool, operation_id, &completed).await?;
                            let copied =
                                copy_file_and_flush_blocking(&self.storage, &source, &handle)
                                    .await?;
                            if copied != expected_size
                                || self.storage.file_length(&handle).map_err(map_storage)?
                                    != expected_size
                            {
                                return Err(FileMutationError::Unavailable);
                            }
                            completed.staging_complete = true;
                            update_pending_payload(&self.pool, operation_id, &completed).await?;
                            handle
                        }
                    };
                    self.storage
                        .rename_no_replace(
                            &staging,
                            &destination_namespace.handle,
                            &destination_name,
                        )
                        .map_err(map_storage)?;
                    completed.result_identity = Some(encode_identity(identity_bytes(&staging)));
                    drop(staging);
                    mark_fs_applied(&self.pool, operation_id, &completed).await?;
                    self.complete(operation_id, project_id, &completed).await?;
                }
            }
            Ok(())
        }

        async fn fail_operation(
            &self,
            operation_id: OperationId,
            code: &'static str,
        ) -> Result<(), FileMutationError> {
            let row = sqlx::query("SELECT project_id, payload FROM operation WHERE id = ?")
                .bind(operation_id.to_string())
                .fetch_optional(&self.pool)
                .await
                .map_err(map_sql)?;
            sqlx::query(
                "UPDATE operation SET state = 'failed', error = ?, updated_at = ?
                 WHERE id = ? AND state IN ('pending', 'fs_applied')",
            )
            .bind(code)
            .bind(timestamp()?)
            .bind(operation_id.to_string())
            .execute(&self.pool)
            .await
            .map_err(map_sql)?;
            if let Some(row) = row {
                let project_id = row
                    .try_get::<String, _>("project_id")
                    .ok()
                    .and_then(|value| canonical::<ProjectId>(value).ok());
                let file_id = row
                    .try_get::<String, _>("payload")
                    .ok()
                    .and_then(|value| serde_json::from_str::<MutationPayload>(&value).ok())
                    .and_then(|payload| canonical::<FileEntryId>(payload.file_id).ok());
                if let (Some(project_id), Some(file_id)) = (project_id, file_id) {
                    let _ = self.reconciliation.try_schedule(ReconciliationRequest {
                        project_id,
                        file_id,
                    });
                }
            }
            Ok(())
        }

        async fn terminalize_failed_copy(
            &self,
            operation_id: OperationId,
            staging: VerifiedHandle,
            error: FileMutationError,
        ) -> Result<FileMutationError, FileMutationError> {
            remove_file_blocking(&self.storage, staging).await?;
            self.fail_operation(operation_id, error.code()).await?;
            Ok(error)
        }
    }

    #[async_trait]
    impl FileMutationSource for ProductionFileMutationSource {
        async fn mutate(
            &self,
            project_id: ProjectId,
            file_id: FileEntryId,
            command: FileMutationCommand,
        ) -> Result<FileEntry, FileMutationError> {
            self.execute(project_id, file_id, command).await
        }
    }

    async fn validate_project(
        pool: &SqlitePool,
        project_id: ProjectId,
    ) -> Result<(), FileMutationError> {
        let row = sqlx::query("SELECT status, deleted_at FROM project WHERE id = ?")
            .bind(project_id.to_string())
            .fetch_optional(pool)
            .await
            .map_err(map_sql)?
            .ok_or(FileMutationError::ProjectNotFound)?;
        let status: String = row.try_get("status").map_err(map_sql)?;
        let deleted: Option<String> = row.try_get("deleted_at").map_err(map_sql)?;
        if deleted.is_some() {
            return Err(FileMutationError::ProjectNotFound);
        }
        if status != "active" {
            return Err(FileMutationError::Unavailable);
        }
        Ok(())
    }

    async fn copy_file_and_flush_blocking(
        storage: &WindowsStorage,
        source: &VerifiedHandle,
        destination: &VerifiedHandle,
    ) -> Result<i64, FileMutationError> {
        let storage = storage.clone();
        let source = source.clone();
        let destination = destination.clone();
        tokio::task::spawn_blocking(move || {
            storage
                .copy_file_and_flush(&source, &destination)
                .map_err(map_storage)
        })
        .await
        .map_err(|_| FileMutationError::Unavailable)?
    }

    async fn remove_file_blocking(
        storage: &WindowsStorage,
        handle: VerifiedHandle,
    ) -> Result<(), FileMutationError> {
        let storage = storage.clone();
        tokio::task::spawn_blocking(move || storage.remove_file(handle).map_err(map_storage))
            .await
            .map_err(|_| FileMutationError::Unavailable)?
    }

    async fn ensure_no_pending_mutation(
        pool: &SqlitePool,
        project_id: ProjectId,
    ) -> Result<(), FileMutationError> {
        let pending: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM operation
             WHERE project_id = ? AND state IN ('pending', 'fs_applied')
               AND kind IN ('file_rename', 'file_move', 'file_copy')",
        )
        .bind(project_id.to_string())
        .fetch_one(pool)
        .await
        .map_err(map_sql)?;
        if pending != 0 {
            return Err(FileMutationError::Unavailable);
        }
        Ok(())
    }

    async fn load_entry(
        pool: &SqlitePool,
        project_id: ProjectId,
        file_id: FileEntryId,
    ) -> Result<CatalogEntry, FileMutationError> {
        let row = sqlx::query("SELECT * FROM file_entry WHERE id = ? AND project_id = ?")
            .bind(file_id.to_string())
            .bind(project_id.to_string())
            .fetch_optional(pool)
            .await
            .map_err(map_sql)?
            .ok_or(FileMutationError::FileNotFound)?;
        row_to_catalog(row)
    }

    async fn load_entry_tx(
        transaction: &mut Transaction<'_, Sqlite>,
        project_id: ProjectId,
        file_id: FileEntryId,
    ) -> Result<CatalogEntry, FileMutationError> {
        let row = sqlx::query("SELECT * FROM file_entry WHERE id = ? AND project_id = ?")
            .bind(file_id.to_string())
            .bind(project_id.to_string())
            .fetch_optional(&mut **transaction)
            .await
            .map_err(map_sql)?
            .ok_or(FileMutationError::FileNotFound)?;
        row_to_catalog(row)
    }

    fn row_to_catalog(row: sqlx::sqlite::SqliteRow) -> Result<CatalogEntry, FileMutationError> {
        let volume: [u8; 8] = row
            .try_get::<Option<Vec<u8>>, _>("volume_serial")
            .map_err(map_sql)?
            .ok_or(FileMutationError::Unavailable)?
            .try_into()
            .map_err(|_| FileMutationError::Unavailable)?;
        let file_id_bytes: [u8; 16] = row
            .try_get::<Option<Vec<u8>>, _>("filesystem_file_id")
            .map_err(map_sql)?
            .ok_or(FileMutationError::Unavailable)?
            .try_into()
            .map_err(|_| FileMutationError::Unavailable)?;
        let mut identity = [0_u8; 24];
        identity[..8].copy_from_slice(&volume);
        identity[8..].copy_from_slice(&file_id_bytes);
        let hash = row
            .try_get::<Option<Vec<u8>>, _>("hash")
            .map_err(map_sql)?
            .map(|bytes| bytes.try_into().map_err(|_| FileMutationError::Unavailable))
            .transpose()?;
        Ok(CatalogEntry {
            id: canonical(row.try_get("id").map_err(map_sql)?)?,
            project_id: canonical(row.try_get("project_id").map_err(map_sql)?)?,
            parent_id: row
                .try_get::<Option<String>, _>("parent_id")
                .map_err(map_sql)?
                .map(canonical)
                .transpose()?,
            exact_name: row.try_get("exact_name").map_err(map_sql)?,
            kind: FileKind::from_str(&row.try_get::<String, _>("kind").map_err(map_sql)?)
                .map_err(|_| FileMutationError::Unavailable)?,
            identity,
            size: row.try_get("size").map_err(map_sql)?,
            mtime: row.try_get("mtime_filetime_100ns").map_err(map_sql)?,
            hash,
            hash_state: FileHashState::from_str(
                &row.try_get::<String, _>("hash_state").map_err(map_sql)?,
            )
            .map_err(|_| FileMutationError::Unavailable)?,
            state: FileState::from_str(&row.try_get::<String, _>("state").map_err(map_sql)?)
                .map_err(|_| FileMutationError::Unavailable)?,
            revision: row.try_get("revision").map_err(map_sql)?,
            scan_generation: row.try_get("scan_generation").map_err(map_sql)?,
            observed_at: OffsetDateTime::parse(
                &row.try_get::<String, _>("observed_at").map_err(map_sql)?,
                &Rfc3339,
            )
            .map_err(|_| FileMutationError::Unavailable)?,
        })
    }

    async fn directory_components(
        pool: &SqlitePool,
        project_id: ProjectId,
        parent_id: Option<FileEntryId>,
    ) -> Result<Vec<String>, FileMutationError> {
        let mut current = parent_id;
        let mut components = Vec::new();
        let mut visited = HashSet::new();
        while let Some(id) = current {
            if components.len() >= MAX_DIRECTORY_DEPTH || !visited.insert(id) {
                return Err(FileMutationError::Unavailable);
            }
            let entry = load_entry(pool, project_id, id).await?;
            if entry.kind != FileKind::Directory || entry.state != FileState::Live {
                return Err(FileMutationError::FileNotFound);
            }
            components.push(entry.exact_name);
            current = entry.parent_id;
        }
        components.reverse();
        Ok(components)
    }

    async fn is_descendant(
        pool: &SqlitePool,
        project_id: ProjectId,
        candidate: Option<FileEntryId>,
        ancestor: FileEntryId,
    ) -> Result<bool, FileMutationError> {
        let mut current = candidate;
        let mut visited = HashSet::new();
        while let Some(id) = current {
            if id == ancestor {
                return Ok(true);
            }
            if !visited.insert(id) || visited.len() > MAX_DIRECTORY_DEPTH {
                return Err(FileMutationError::Unavailable);
            }
            current = load_entry(pool, project_id, id).await?.parent_id;
        }
        Ok(false)
    }

    async fn names_equal_ci(
        pool: &SqlitePool,
        left: &str,
        right: &str,
    ) -> Result<bool, FileMutationError> {
        let equal: i64 = sqlx::query_scalar(
            "SELECT CASE WHEN ? = ? COLLATE WINDOWS_ORDINAL_CI_V1 THEN 1 ELSE 0 END",
        )
        .bind(left)
        .bind(right)
        .fetch_one(pool)
        .await
        .map_err(map_sql)?;
        Ok(equal == 1)
    }

    async fn ensure_catalog_destination_absent(
        pool: &SqlitePool,
        project_id: ProjectId,
        parent_id: Option<FileEntryId>,
        name: &str,
        excluded: Option<FileEntryId>,
    ) -> Result<(), FileMutationError> {
        let row: Option<String> = sqlx::query_scalar(
            "SELECT id FROM file_entry
             WHERE project_id = ? AND parent_id IS ? AND exact_name = ?
               AND state IN ('live', 'settling', 'unsupported') LIMIT 1",
        )
        .bind(project_id.to_string())
        .bind(parent_id.map(|id| id.to_string()))
        .bind(name)
        .fetch_optional(pool)
        .await
        .map_err(map_sql)?;
        if row.is_some_and(|id| excluded.is_none_or(|excluded| id != excluded.to_string())) {
            return Err(FileMutationError::Conflict);
        }
        Ok(())
    }

    fn ensure_physical_destination_absent(
        storage: &WindowsStorage,
        parent: &VerifiedHandle,
        name: &WindowsName,
        allowed_identity: Option<[u8; 24]>,
    ) -> Result<(), FileMutationError> {
        match storage.open_verified(parent, name) {
            Ok(handle) if allowed_identity == Some(identity_bytes(&handle)) => Ok(()),
            Ok(_) => Err(FileMutationError::Conflict),
            Err(error) if error.kind() == StorageErrorKind::NotFound => Ok(()),
            Err(error) => Err(map_storage(error)),
        }
    }

    fn open_for_mutation_if_present(
        storage: &WindowsStorage,
        parent: &VerifiedHandle,
        name: &WindowsName,
    ) -> Result<Option<VerifiedHandle>, FileMutationError> {
        match storage.open_verified_for_namespace_mutation(parent, name) {
            Ok(handle) => Ok(Some(handle)),
            Err(error) if error.kind() == StorageErrorKind::NotFound => Ok(None),
            Err(error) => Err(map_storage(error)),
        }
    }

    fn open_writable_if_present(
        storage: &WindowsStorage,
        parent: &VerifiedHandle,
        name: &WindowsName,
    ) -> Result<Option<VerifiedHandle>, FileMutationError> {
        match storage.open_verified_writable(parent, name) {
            Ok(handle) => Ok(Some(handle)),
            Err(error) if error.kind() == StorageErrorKind::NotFound => Ok(None),
            Err(error) => Err(map_storage(error)),
        }
    }

    async fn insert_operation(
        pool: &SqlitePool,
        operation_id: OperationId,
        project_id: ProjectId,
        payload: &MutationPayload,
    ) -> Result<(), FileMutationError> {
        let now = timestamp()?;
        sqlx::query(
            "INSERT INTO operation
             (id, project_id, kind, state, payload_version, payload, error, created_at, updated_at)
             VALUES (?, ?, ?, 'pending', ?, ?, NULL, ?, ?)",
        )
        .bind(operation_id.to_string())
        .bind(project_id.to_string())
        .bind(payload.mutation.operation_kind())
        .bind(PAYLOAD_VERSION)
        .bind(serde_json::to_string(payload).map_err(|_| FileMutationError::Unavailable)?)
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await
        .map_err(map_sql)?;
        Ok(())
    }

    async fn mark_fs_applied(
        pool: &SqlitePool,
        operation_id: OperationId,
        payload: &MutationPayload,
    ) -> Result<(), FileMutationError> {
        sqlx::query(
            "UPDATE operation SET state = 'fs_applied', payload = ?, updated_at = ?
             WHERE id = ? AND state IN ('pending', 'fs_applied')",
        )
        .bind(serde_json::to_string(payload).map_err(|_| FileMutationError::Unavailable)?)
        .bind(timestamp()?)
        .bind(operation_id.to_string())
        .execute(pool)
        .await
        .map_err(map_sql)?;
        Ok(())
    }

    async fn update_pending_payload(
        pool: &SqlitePool,
        operation_id: OperationId,
        payload: &MutationPayload,
    ) -> Result<(), FileMutationError> {
        let affected = sqlx::query(
            "UPDATE operation SET payload = ?, updated_at = ?
             WHERE id = ? AND state = 'pending'",
        )
        .bind(serde_json::to_string(payload).map_err(|_| FileMutationError::Unavailable)?)
        .bind(timestamp()?)
        .bind(operation_id.to_string())
        .execute(pool)
        .await
        .map_err(map_sql)?
        .rows_affected();
        if affected != 1 {
            return Err(FileMutationError::Unavailable);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_copy(
        transaction: &mut Transaction<'_, Sqlite>,
        source: &CatalogEntry,
        result_id: FileEntryId,
        parent_id: Option<FileEntryId>,
        name: &str,
        identity: [u8; 24],
        size: i64,
        mtime: i64,
        now: &str,
    ) -> Result<(), FileMutationError> {
        sqlx::query(
            "INSERT INTO file_entry
             (id, project_id, parent_id, exact_name, kind, platform_kind, volume_serial,
              filesystem_file_id, size, mtime_filetime_100ns, hash, hash_state, state,
              revision, scan_generation, observed_at)
             VALUES (?, ?, ?, ?, 'file', 'windows_file_id', ?, ?, ?, ?, ?, ?, 'live', 1, 0, ?)",
        )
        .bind(result_id.to_string())
        .bind(source.project_id.to_string())
        .bind(parent_id.map(|id| id.to_string()))
        .bind(name)
        .bind(identity[..8].to_vec())
        .bind(identity[8..].to_vec())
        .bind(size)
        .bind(mtime)
        .bind(source.hash.map(|hash| hash.to_vec()))
        .bind(match source.hash_state {
            FileHashState::Unknown => "unknown",
            FileHashState::Queued => "queued",
            FileHashState::Computing => "computing",
            FileHashState::Ready => "ready",
            FileHashState::Failed => "failed",
        })
        .bind(now)
        .execute(&mut **transaction)
        .await
        .map_err(map_sql)?;
        Ok(())
    }

    fn entry_to_public(
        entry: CatalogEntry,
        parent_components: Vec<String>,
    ) -> Result<FileEntry, FileMutationError> {
        let mut relative_path = parent_components.join("/");
        if !relative_path.is_empty() {
            relative_path.push('/');
        }
        relative_path.push_str(&entry.exact_name);
        Ok(FileEntry {
            id: entry.id,
            project_id: entry.project_id,
            parent_id: entry.parent_id,
            exact_name: FileExactName::parse(entry.exact_name)
                .map_err(|_| FileMutationError::Unavailable)?,
            relative_path,
            kind: entry.kind,
            platform_identity: PlatformIdentity::try_new(
                "windows_file_id",
                Some(entry.identity[..8].to_vec()),
                Some(entry.identity[8..].to_vec()),
            )
            .map_err(|_| FileMutationError::Unavailable)?,
            size: entry.size,
            mtime_filetime_100ns: entry.mtime,
            hash: entry.hash,
            hash_state: entry.hash_state,
            state: entry.state,
            revision: entry.revision,
            scan_generation: entry.scan_generation,
            observed_at: entry.observed_at,
        })
    }

    fn identity_bytes(handle: &VerifiedHandle) -> [u8; 24] {
        let identity = handle.identity();
        let mut bytes = [0_u8; 24];
        bytes[..8].copy_from_slice(&identity.volume_serial.to_le_bytes());
        bytes[8..].copy_from_slice(&identity.file_id.to_le_bytes());
        bytes
    }

    fn encode_identity(identity: [u8; 24]) -> String {
        identity.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn decode_identity(value: &str) -> Result<[u8; 24], FileMutationError> {
        if value.len() != 48 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(FileMutationError::Unavailable);
        }
        let mut identity = [0_u8; 24];
        for (index, byte) in identity.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
                .map_err(|_| FileMutationError::Unavailable)?;
        }
        Ok(identity)
    }

    fn canonical<T>(value: String) -> Result<T, FileMutationError>
    where
        T: FromStr + ToString,
    {
        let parsed = value
            .parse::<T>()
            .map_err(|_| FileMutationError::Unavailable)?;
        if parsed.to_string() != value {
            return Err(FileMutationError::Unavailable);
        }
        Ok(parsed)
    }

    fn timestamp() -> Result<String, FileMutationError> {
        OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .map_err(|_| FileMutationError::Unavailable)
    }

    fn map_storage(error: StorageError) -> FileMutationError {
        match error.kind() {
            StorageErrorKind::InvalidName => FileMutationError::InvalidName,
            StorageErrorKind::NotFound => FileMutationError::FileNotFound,
            StorageErrorKind::Conflict => FileMutationError::Conflict,
            StorageErrorKind::Unsupported => FileMutationError::Unsupported,
            StorageErrorKind::InsufficientStorage => FileMutationError::InsufficientStorage,
            StorageErrorKind::AccessDenied
            | StorageErrorKind::CleanupFailed
            | StorageErrorKind::WorkerFailed
            | StorageErrorKind::Io => FileMutationError::Unavailable,
        }
    }

    fn map_sql(_: sqlx::Error) -> FileMutationError {
        FileMutationError::Unavailable
    }
}

#[cfg(windows)]
pub use platform::*;

#[cfg(not(windows))]
mod platform_stub {
    use std::sync::Arc;

    use async_trait::async_trait;
    use cellar_api::routes::files::{FileMutationCommand, FileMutationError, FileMutationSource};
    use cellar_core::{FileEntry, FileEntryId, ProjectId, ProjectMutationCoordinator};
    use cellar_windows::WindowsStorage;
    use sqlx::SqlitePool;

    use crate::downloads::ReconciliationScheduler;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum FileMutationFaultPoint {
        AfterIntent,
        AfterTemporaryRename,
        AfterStagingIntent,
        CopyIoFailure,
        AfterCopyStaging,
        AfterFilesystemApply,
    }

    pub trait FileMutationFaultInjector: Send + Sync {
        fn should_fail(&self, point: FileMutationFaultPoint) -> bool;
    }

    #[derive(Clone)]
    pub struct ProductionFileMutationSource;

    impl ProductionFileMutationSource {
        pub async fn open(_: SqlitePool, _: WindowsStorage) -> Result<Self, FileMutationError> {
            Err(FileMutationError::Unsupported)
        }

        pub async fn open_with_fault_injector(
            _: SqlitePool,
            _: WindowsStorage,
            _: Arc<dyn FileMutationFaultInjector>,
        ) -> Result<Self, FileMutationError> {
            Err(FileMutationError::Unsupported)
        }

        pub async fn open_with_reconciliation(
            _: SqlitePool,
            _: WindowsStorage,
            _: Arc<dyn ReconciliationScheduler>,
            _: Arc<dyn ProjectMutationCoordinator>,
        ) -> Result<Self, FileMutationError> {
            Err(FileMutationError::Unsupported)
        }

        pub async fn open_with_fault_injector_and_reconciliation(
            _: SqlitePool,
            _: WindowsStorage,
            _: Arc<dyn FileMutationFaultInjector>,
            _: Arc<dyn ReconciliationScheduler>,
            _: Arc<dyn ProjectMutationCoordinator>,
        ) -> Result<Self, FileMutationError> {
            Err(FileMutationError::Unsupported)
        }

        pub async fn recover_pending(&self) -> Result<(), FileMutationError> {
            Err(FileMutationError::Unsupported)
        }
    }

    #[async_trait]
    impl FileMutationSource for ProductionFileMutationSource {
        async fn mutate(
            &self,
            _: ProjectId,
            _: FileEntryId,
            _: FileMutationCommand,
        ) -> Result<FileEntry, FileMutationError> {
            Err(FileMutationError::Unsupported)
        }
    }
}

#[cfg(not(windows))]
pub use platform_stub::*;
