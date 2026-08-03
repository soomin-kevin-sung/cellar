mod common;

use std::{
    io::{self, Write},
    sync::{Arc, Mutex, OnceLock},
};

use cellar::{
    db::{Database, DbError, NewUpload, ProjectRow, UploadRow, UploadState},
    storage::SafeFileName,
    uploads::{UploadRepository, UploadRepositoryFuture, UploadService},
};
use uuid::Uuid;

use common::TestContext;

async fn project(context: &TestContext) -> Uuid {
    let id = Uuid::now_v7();
    context.storage.create_project_dir(id).await.unwrap();
    context
        .database
        .create_project(cellar::db::NewProject::new(id, "Recovery").unwrap())
        .await
        .unwrap();
    id
}

async fn upload(context: &TestContext, project_id: Uuid, total: u64) -> Uuid {
    let id = Uuid::now_v7();
    context.storage.create_staging(id).await.unwrap();
    context
        .database
        .create_upload(NewUpload::new(id, project_id, "recover.bin", total).unwrap())
        .await
        .unwrap();
    id
}

async fn append(context: &TestContext, upload_id: Uuid, bytes: &'static [u8]) {
    context
        .storage
        .write_chunk(upload_id, 0, bytes)
        .await
        .unwrap();
}

#[tokio::test]
async fn recovery_repairs_active_trailing_bytes_and_fails_short_or_missing_staging() {
    let context = TestContext::new().await;
    let project_id = project(&context).await;

    let trailing = upload(&context, project_id, 4).await;
    append(&context, trailing, b"dataTAIL").await;
    assert!(
        context
            .database
            .advance_offset(trailing, 0, 4)
            .await
            .unwrap()
    );

    let short = upload(&context, project_id, 4).await;
    append(&context, short, b"dat").await;
    assert!(context.database.advance_offset(short, 0, 4).await.unwrap());

    let missing = upload(&context, project_id, 4).await;
    assert!(
        context
            .database
            .advance_offset(missing, 0, 4)
            .await
            .unwrap()
    );
    context.storage.remove_staging(missing).await.unwrap();

    UploadService::new(Arc::new(context.database.clone()), context.storage.clone())
        .recover_uploads()
        .await
        .unwrap();

    assert_eq!(
        context.storage.staging_len(trailing).await.unwrap(),
        Some(4)
    );
    assert_eq!(
        context
            .database
            .get_upload(trailing)
            .await
            .unwrap()
            .unwrap()
            .state(),
        UploadState::Active
    );
    for id in [short, missing] {
        assert_eq!(
            context
                .database
                .get_upload(id)
                .await
                .unwrap()
                .unwrap()
                .state(),
            UploadState::Failed
        );
    }
    context.close().await;
}

#[tokio::test]
async fn recovery_completes_both_finalizing_crash_windows_and_is_idempotent() {
    let context = TestContext::new().await;
    let project_id = project(&context).await;
    let name = SafeFileName::parse("recover.bin").unwrap();

    let moved = upload(&context, project_id, 4).await;
    append(&context, moved, b"move").await;
    assert!(context.database.advance_offset(moved, 0, 4).await.unwrap());
    assert!(context.database.mark_finalizing(moved, 4).await.unwrap());
    context
        .storage
        .finalize_no_replace(moved, project_id, &name)
        .await
        .unwrap();

    let staged_project = project(&context).await;
    let staged_id = upload(&context, staged_project, 4).await;
    append(&context, staged_id, b"data").await;
    assert!(
        context
            .database
            .advance_offset(staged_id, 0, 4)
            .await
            .unwrap()
    );
    assert!(
        context
            .database
            .mark_finalizing(staged_id, 4)
            .await
            .unwrap()
    );
    let service = UploadService::new(Arc::new(context.database.clone()), context.storage.clone());
    service.recover_uploads().await.unwrap();
    service.recover_uploads().await.unwrap();

    for id in [moved, staged_id] {
        assert_eq!(
            context
                .database
                .get_upload(id)
                .await
                .unwrap()
                .unwrap()
                .state(),
            UploadState::Complete
        );
        assert_eq!(context.storage.staging_len(id).await.unwrap(), None);
    }
    assert_eq!(
        std::fs::read(context.project_path(project_id).join("files/recover.bin")).unwrap(),
        b"move"
    );
    assert_eq!(
        std::fs::read(
            context
                .project_path(staged_project)
                .join("files/recover.bin")
        )
        .unwrap(),
        b"data"
    );
    context.close().await;
}

#[tokio::test]
async fn recovery_preserves_finalizing_conflict_and_zero_byte_boundaries() {
    let context = TestContext::new().await;
    let conflict_project = project(&context).await;
    let conflict = upload(&context, conflict_project, 4).await;
    append(&context, conflict, b"data").await;
    assert!(
        context
            .database
            .advance_offset(conflict, 0, 4)
            .await
            .unwrap()
    );
    assert!(context.database.mark_finalizing(conflict, 4).await.unwrap());
    let conflict_path = context
        .project_path(conflict_project)
        .join("files/recover.bin");
    std::fs::write(&conflict_path, b"preserve").unwrap();

    let zero_project = project(&context).await;
    let zero = upload(&context, zero_project, 0).await;
    assert!(context.database.mark_finalizing(zero, 0).await.unwrap());

    UploadService::new(Arc::new(context.database.clone()), context.storage.clone())
        .recover_uploads()
        .await
        .unwrap();

    assert_eq!(std::fs::read(conflict_path).unwrap(), b"preserve");
    assert_eq!(
        context.storage.staging_len(conflict).await.unwrap(),
        Some(4)
    );
    assert_eq!(
        context
            .database
            .get_upload(conflict)
            .await
            .unwrap()
            .unwrap()
            .state(),
        UploadState::Failed
    );
    assert_eq!(
        context
            .database
            .get_upload(zero)
            .await
            .unwrap()
            .unwrap()
            .state(),
        UploadState::Complete
    );
    assert_eq!(
        std::fs::metadata(context.project_path(zero_project).join("files/recover.bin"))
            .unwrap()
            .len(),
        0
    );
    context.close().await;
}

#[tokio::test]
async fn recovery_covers_unchanged_active_destination_conflicts_missing_files_and_terminals() {
    let context = TestContext::new().await;

    let unchanged_project = project(&context).await;
    let unchanged = upload(&context, unchanged_project, 4).await;
    append(&context, unchanged, b"data").await;
    assert!(
        context
            .database
            .advance_offset(unchanged, 0, 4)
            .await
            .unwrap()
    );

    let active_conflict_project = project(&context).await;
    let active_conflict = upload(&context, active_conflict_project, 0).await;
    let active_destination = context
        .project_path(active_conflict_project)
        .join("files/recover.bin");
    std::fs::write(&active_destination, b"active destination").unwrap();

    let absent_project = project(&context).await;
    let absent = upload(&context, absent_project, 0).await;
    assert!(context.database.mark_finalizing(absent, 0).await.unwrap());
    context.storage.remove_staging(absent).await.unwrap();

    let wrong_project = project(&context).await;
    let wrong = upload(&context, wrong_project, 4).await;
    append(&context, wrong, b"data").await;
    assert!(context.database.advance_offset(wrong, 0, 4).await.unwrap());
    assert!(context.database.mark_finalizing(wrong, 4).await.unwrap());
    context.storage.remove_staging(wrong).await.unwrap();
    let wrong_destination = context
        .project_path(wrong_project)
        .join("files/recover.bin");
    std::fs::write(&wrong_destination, b"bad").unwrap();

    let complete_project = project(&context).await;
    let complete = upload(&context, complete_project, 0).await;
    assert!(context.database.mark_finalizing(complete, 0).await.unwrap());
    context
        .storage
        .finalize_no_replace(
            complete,
            complete_project,
            &SafeFileName::parse("recover.bin").unwrap(),
        )
        .await
        .unwrap();
    context.database.mark_complete(complete).await.unwrap();

    let failed_project = project(&context).await;
    let failed = upload(&context, failed_project, 0).await;
    context
        .database
        .mark_failed(failed, "fixture terminal")
        .await
        .unwrap();
    let unrelated = context
        .project_path(failed_project)
        .join("files/unrelated.txt");
    std::fs::write(&unrelated, b"do not scan or change").unwrap();

    UploadService::new(Arc::new(context.database.clone()), context.storage.clone())
        .recover_uploads()
        .await
        .unwrap();

    assert_eq!(
        context
            .database
            .get_upload(unchanged)
            .await
            .unwrap()
            .unwrap()
            .state(),
        UploadState::Active
    );
    assert_eq!(
        context.storage.staging_len(unchanged).await.unwrap(),
        Some(4)
    );
    for id in [active_conflict, absent, wrong] {
        assert_eq!(
            context
                .database
                .get_upload(id)
                .await
                .unwrap()
                .unwrap()
                .state(),
            UploadState::Failed
        );
    }
    assert_eq!(
        std::fs::read(active_destination).unwrap(),
        b"active destination"
    );
    assert_eq!(std::fs::read(wrong_destination).unwrap(), b"bad");
    assert_eq!(
        context
            .database
            .get_upload(complete)
            .await
            .unwrap()
            .unwrap()
            .state(),
        UploadState::Complete
    );
    assert_eq!(
        context
            .database
            .get_upload(failed)
            .await
            .unwrap()
            .unwrap()
            .state(),
        UploadState::Failed
    );
    assert_eq!(std::fs::read(unrelated).unwrap(), b"do not scan or change");
    context.close().await;
}

#[derive(Clone)]
struct CapturedWriter(Arc<Mutex<Vec<u8>>>);

impl Write for CapturedWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

static CAPTURED_LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
static LOG_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn captured_logs() -> Arc<Mutex<Vec<u8>>> {
    CAPTURED_LOGS
        .get_or_init(|| {
            let logs = Arc::new(Mutex::new(Vec::new()));
            let writer = CapturedWriter(logs.clone());
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish();
            tracing::subscriber::set_global_default(subscriber).unwrap();
            logs
        })
        .clone()
}

#[tokio::test]
async fn recovery_logs_one_closed_safe_outcome_per_repaired_or_failed_session() {
    let _log_guard = LOG_TEST_LOCK.lock().await;
    let logs = captured_logs();
    logs.lock().unwrap().clear();
    let context = TestContext::new().await;
    let project_id = project(&context).await;
    let repaired = upload(&context, project_id, 4).await;
    append(&context, repaired, b"dataTAIL").await;
    assert!(
        context
            .database
            .advance_offset(repaired, 0, 4)
            .await
            .unwrap()
    );
    let failed_project = project(&context).await;
    let failed = upload(&context, failed_project, 4).await;
    context.storage.remove_staging(failed).await.unwrap();
    let unchanged_project = project(&context).await;
    let unchanged = upload(&context, unchanged_project, 0).await;

    UploadService::new(Arc::new(context.database.clone()), context.storage.clone())
        .recover_uploads()
        .await
        .unwrap();

    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    for (id, outcome, reason) in [
        (repaired, "repaired", "active_staging_truncated"),
        (failed, "failed", "active_staging_missing"),
    ] {
        let matching = logs
            .lines()
            .filter(|line| line.contains(&id.to_string()))
            .collect::<Vec<_>>();
        assert_eq!(
            matching.len(),
            1,
            "unexpected events for {id}: {matching:?}"
        );
        assert!(matching[0].contains(outcome));
        assert!(matching[0].contains(reason));
    }
    assert!(!logs.contains(&unchanged.to_string()));
    for secret in [
        "recover.bin",
        "Recovery",
        context.temp.path().to_string_lossy().as_ref(),
    ] {
        assert!(!logs.contains(secret), "logs contained {secret}");
    }
    context.close().await;
}

struct QueryFailingRepository;

impl UploadRepository for QueryFailingRepository {
    fn get_project<'a>(&'a self, _: Uuid) -> UploadRepositoryFuture<'a, Option<ProjectRow>> {
        Box::pin(async { Err(DbError::InvalidTransition) })
    }

    fn create_upload<'a>(&'a self, _: NewUpload) -> UploadRepositoryFuture<'a, UploadRow> {
        Box::pin(async { Err(DbError::InvalidTransition) })
    }

    fn get_upload<'a>(&'a self, _: Uuid) -> UploadRepositoryFuture<'a, Option<UploadRow>> {
        Box::pin(async { Err(DbError::InvalidTransition) })
    }

    fn recoverable_uploads(&self) -> UploadRepositoryFuture<'_, Vec<UploadRow>> {
        Box::pin(async { Err(DbError::InvalidTransition) })
    }
}

#[tokio::test]
async fn recovery_database_ambiguity_returns_error_and_logs_only_a_closed_reason() {
    let _log_guard = LOG_TEST_LOCK.lock().await;
    let logs = captured_logs();
    logs.lock().unwrap().clear();
    let context = TestContext::new().await;
    let result = UploadService::new(Arc::new(QueryFailingRepository), context.storage.clone())
        .recover_uploads()
        .await;

    assert!(result.is_err());
    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    let matching = logs
        .lines()
        .filter(|line| line.contains("recoverable_query_failed"))
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "unexpected recovery error logs: {logs}");
    assert!(matching[0].contains("outcome=\"error\""));
    assert!(!logs.contains(context.temp.path().to_string_lossy().as_ref()));
    context.close().await;
}

#[tokio::test]
async fn recovery_preserves_unsafe_exact_entries_fails_sessions_and_continues() {
    let _log_guard = LOG_TEST_LOCK.lock().await;
    let logs = captured_logs();
    logs.lock().unwrap().clear();
    let context = TestContext::new().await;

    let unsafe_staging_project = project(&context).await;
    let unsafe_staging = upload(&context, unsafe_staging_project, 0).await;
    let unsafe_staging_path = context
        .temp
        .path()
        .join(".cellar/uploads")
        .join(format!("{unsafe_staging}.part"));
    context
        .storage
        .remove_staging(unsafe_staging)
        .await
        .unwrap();
    std::fs::create_dir(&unsafe_staging_path).unwrap();

    let unsafe_destination_project = project(&context).await;
    let unsafe_destination = upload(&context, unsafe_destination_project, 0).await;
    assert!(
        context
            .database
            .mark_finalizing(unsafe_destination, 0)
            .await
            .unwrap()
    );
    let unsafe_destination_path = context
        .project_path(unsafe_destination_project)
        .join("files/recover.bin");
    std::fs::create_dir(&unsafe_destination_path).unwrap();

    let later_project = project(&context).await;
    let later = upload(&context, later_project, 0).await;
    context.storage.remove_staging(later).await.unwrap();

    UploadService::new(Arc::new(context.database.clone()), context.storage.clone())
        .recover_uploads()
        .await
        .unwrap();

    for id in [unsafe_staging, unsafe_destination, later] {
        let row = context.database.get_upload(id).await.unwrap().unwrap();
        assert_eq!(row.state(), UploadState::Failed);
        if id != later {
            assert_eq!(row.failure_reason(), Some("unsafe_recovery_entry"));
        }
    }
    assert!(std::fs::metadata(&unsafe_staging_path).unwrap().is_dir());
    assert!(
        std::fs::metadata(&unsafe_destination_path)
            .unwrap()
            .is_dir()
    );

    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    for id in [unsafe_staging, unsafe_destination] {
        let matching = logs
            .lines()
            .filter(|line| line.contains(&id.to_string()))
            .collect::<Vec<_>>();
        assert_eq!(
            matching.len(),
            1,
            "unexpected events for {id}: {matching:?}"
        );
        assert!(matching[0].contains("outcome=\"failed\""));
        assert!(matching[0].contains("unsafe_recovery_entry"));
    }
    for secret in [
        "recover.bin",
        unsafe_staging_path.to_string_lossy().as_ref(),
        unsafe_destination_path.to_string_lossy().as_ref(),
    ] {
        assert!(!logs.contains(secret), "logs contained {secret}");
    }
    context.close().await;
}

struct MarkFailedFailingRepository {
    database: Database,
}

impl UploadRepository for MarkFailedFailingRepository {
    fn get_project<'a>(
        &'a self,
        project_id: Uuid,
    ) -> UploadRepositoryFuture<'a, Option<ProjectRow>> {
        Box::pin(async move { self.database.get_project(project_id).await })
    }

    fn create_upload<'a>(&'a self, upload: NewUpload) -> UploadRepositoryFuture<'a, UploadRow> {
        Box::pin(async move { self.database.create_upload(upload).await })
    }

    fn get_upload<'a>(&'a self, upload_id: Uuid) -> UploadRepositoryFuture<'a, Option<UploadRow>> {
        Box::pin(async move { self.database.get_upload(upload_id).await })
    }

    fn mark_upload_failed<'a>(&'a self, _: Uuid, _: &'a str) -> UploadRepositoryFuture<'a, ()> {
        Box::pin(async { Err(DbError::InvalidTransition) })
    }

    fn recoverable_uploads(&self) -> UploadRepositoryFuture<'_, Vec<UploadRow>> {
        Box::pin(async move { self.database.recoverable_uploads().await })
    }
}

#[tokio::test]
async fn unsafe_entry_failure_transition_ambiguity_aborts_without_false_failed_outcome() {
    let _log_guard = LOG_TEST_LOCK.lock().await;
    let logs = captured_logs();
    logs.lock().unwrap().clear();
    let context = TestContext::new().await;
    let project_id = project(&context).await;
    let upload_id = upload(&context, project_id, 0).await;
    let staging_path = context
        .temp
        .path()
        .join(".cellar/uploads")
        .join(format!("{upload_id}.part"));
    context.storage.remove_staging(upload_id).await.unwrap();
    std::fs::create_dir(&staging_path).unwrap();

    let result = UploadService::new(
        Arc::new(MarkFailedFailingRepository {
            database: context.database.clone(),
        }),
        context.storage.clone(),
    )
    .recover_uploads()
    .await;

    assert!(result.is_err());
    assert_eq!(
        context
            .database
            .get_upload(upload_id)
            .await
            .unwrap()
            .unwrap()
            .state(),
        UploadState::Active
    );
    assert!(std::fs::metadata(&staging_path).unwrap().is_dir());
    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    let matching = logs
        .lines()
        .filter(|line| line.contains(&upload_id.to_string()))
        .collect::<Vec<_>>();
    assert_eq!(
        matching.len(),
        1,
        "unexpected events for {upload_id}: {matching:?}"
    );
    assert!(matching[0].contains("outcome=\"error\""));
    assert!(!matching[0].contains("outcome=\"failed\""));
    assert!(!logs.contains("recover.bin"));
    assert!(!logs.contains(staging_path.to_string_lossy().as_ref()));
    context.close().await;
}

#[tokio::test]
async fn recovery_managed_upload_directory_corruption_remains_fatal() {
    let context = TestContext::new().await;
    let project_id = project(&context).await;
    let upload_id = upload(&context, project_id, 0).await;
    context.storage.remove_staging(upload_id).await.unwrap();
    let uploads_dir = context.temp.path().join(".cellar/uploads");
    std::fs::remove_dir(&uploads_dir).unwrap();
    std::fs::write(&uploads_dir, b"unsafe managed directory").unwrap();

    let result = UploadService::new(Arc::new(context.database.clone()), context.storage.clone())
        .recover_uploads()
        .await;

    assert!(result.is_err());
    assert_eq!(
        context
            .database
            .get_upload(upload_id)
            .await
            .unwrap()
            .unwrap()
            .state(),
        UploadState::Active
    );
    assert_eq!(
        std::fs::read(uploads_dir).unwrap(),
        b"unsafe managed directory"
    );
    context.close().await;
}
