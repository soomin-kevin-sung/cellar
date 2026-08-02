#![cfg(windows)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cellar_api::routes::files::{FileMutationCommand, FileMutationError, FileMutationSource};
use cellar_core::{FileEntryId, FileExactName, InMemoryProjectMutationCoordinator, ProjectId};
use cellar_db::{FilenameCollation, migrate, open_pool};
use cellar_service::downloads::{ReconciliationRequest, ReconciliationScheduler};
use cellar_service::file_mutations::{
    FileMutationFaultInjector, FileMutationFaultPoint, ProductionFileMutationSource,
};
use cellar_windows::{VerifiedHandle, WindowsName, WindowsStorage};
use sqlx::{Row, SqlitePool};
use tempfile::TempDir;

struct Fixture {
    _database: TempDir,
    root: TempDir,
    pool: SqlitePool,
    storage: WindowsStorage,
    project_id: ProjectId,
    file_id: FileEntryId,
    folder_id: FileEntryId,
}

impl Fixture {
    async fn new() -> Self {
        let root = TempDir::new().unwrap();
        let project_id = ProjectId::new();
        let files = root
            .path()
            .join("projects")
            .join(project_id.to_string())
            .join("files");
        std::fs::create_dir_all(files.join("folder")).unwrap();
        std::fs::write(files.join("alpha.txt"), b"alpha").unwrap();
        std::fs::write(files.join("occupied.txt"), b"external").unwrap();

        let identity = cellar_windows::preflight::open_as_service(root.path()).unwrap();
        let storage = WindowsStorage::adopt(identity).unwrap();
        let files_handle = open_components(
            &storage,
            storage.root(),
            &["projects", &project_id.to_string(), "files"],
        );
        let source = storage
            .open_verified(&files_handle, &WindowsName::parse("alpha.txt").unwrap())
            .unwrap();
        let folder = storage
            .open_verified(&files_handle, &WindowsName::parse("folder").unwrap())
            .unwrap();
        let (_, source_mtime) = storage.file_length_and_mtime(&source).unwrap();

        let database = TempDir::new().unwrap();
        let pool = open_pool(
            database.path().join("cellar.db"),
            FilenameCollation::windows_ordinal_ci_v1(|left, right| {
                left.to_lowercase().cmp(&right.to_lowercase())
            }),
        )
        .await
        .unwrap();
        migrate(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO project
             (id, name, description, status, version, created_at, updated_at)
             VALUES (?, 'Mutation fixture', '', 'active', 1,
                     '2026-08-03T00:00:00Z', '2026-08-03T00:00:00Z')",
        )
        .bind(project_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
        let folder_id = FileEntryId::new();
        insert_entry(
            &pool,
            EntrySeed {
                id: folder_id,
                project_id,
                parent_id: None,
                name: "folder",
                kind: "directory",
                handle: &folder,
                size: 0,
                mtime: 0,
            },
        )
        .await;
        let file_id = FileEntryId::new();
        insert_entry(
            &pool,
            EntrySeed {
                id: file_id,
                project_id,
                parent_id: None,
                name: "alpha.txt",
                kind: "file",
                handle: &source,
                size: 5,
                mtime: source_mtime,
            },
        )
        .await;

        Self {
            _database: database,
            root,
            pool,
            storage,
            project_id,
            file_id,
            folder_id,
        }
    }

    fn files(&self) -> std::path::PathBuf {
        self.root
            .path()
            .join("projects")
            .join(self.project_id.to_string())
            .join("files")
    }
}

fn open_components(
    storage: &WindowsStorage,
    root: &VerifiedHandle,
    components: &[&str],
) -> VerifiedHandle {
    let mut current = root.clone();
    for component in components {
        current = storage
            .open_verified(&current, &WindowsName::parse(*component).unwrap())
            .unwrap();
    }
    current
}

fn exact_names(directory: &std::path::Path) -> Vec<String> {
    let mut names = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

struct EntrySeed<'a> {
    id: FileEntryId,
    project_id: ProjectId,
    parent_id: Option<FileEntryId>,
    name: &'a str,
    kind: &'a str,
    handle: &'a VerifiedHandle,
    size: i64,
    mtime: i64,
}

async fn insert_entry(pool: &SqlitePool, seed: EntrySeed<'_>) {
    let identity = seed.handle.identity();
    sqlx::query(
        "INSERT INTO file_entry
         (id, project_id, parent_id, exact_name, kind, platform_kind, volume_serial,
          filesystem_file_id, size, mtime_filetime_100ns, hash, hash_state, state,
          revision, scan_generation, observed_at)
         VALUES (?, ?, ?, ?, ?, 'windows_file_id', ?, ?, ?, ?, NULL, 'unknown',
                 'live', 1, 0, '2026-08-03T00:00:00Z')",
    )
    .bind(seed.id.to_string())
    .bind(seed.project_id.to_string())
    .bind(seed.parent_id.map(|id| id.to_string()))
    .bind(seed.name)
    .bind(seed.kind)
    .bind(identity.volume_serial.to_le_bytes().to_vec())
    .bind(identity.file_id.to_le_bytes().to_vec())
    .bind(seed.size)
    .bind(seed.mtime)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn rename_move_and_copy_preserve_no_overwrite_and_catalog_identity() {
    let fixture = Fixture::new().await;
    let source = ProductionFileMutationSource::open(fixture.pool.clone(), fixture.storage.clone())
        .await
        .unwrap();

    let renamed = source
        .mutate(
            fixture.project_id,
            fixture.file_id,
            FileMutationCommand::Rename {
                expected_revision: 1,
                name: FileExactName::parse("ALPHA.TXT").unwrap(),
            },
        )
        .await
        .unwrap();
    assert_eq!(renamed.exact_name.as_str(), "ALPHA.TXT");
    assert_eq!(renamed.revision, 2);
    assert!(
        !exact_names(&fixture.files())
            .iter()
            .any(|name| name == "alpha.txt")
    );
    assert!(
        exact_names(&fixture.files())
            .iter()
            .any(|name| name == "ALPHA.TXT")
    );
    assert_eq!(
        std::fs::read(fixture.files().join("ALPHA.TXT")).unwrap(),
        b"alpha"
    );

    let conflict = source
        .mutate(
            fixture.project_id,
            fixture.file_id,
            FileMutationCommand::Rename {
                expected_revision: 2,
                name: FileExactName::parse("occupied.txt").unwrap(),
            },
        )
        .await;
    assert_eq!(conflict.unwrap_err(), FileMutationError::Conflict);
    assert_eq!(
        std::fs::read(fixture.files().join("occupied.txt")).unwrap(),
        b"external"
    );
    assert_eq!(
        std::fs::read(fixture.files().join("ALPHA.TXT")).unwrap(),
        b"alpha"
    );

    let moved = source
        .mutate(
            fixture.project_id,
            fixture.file_id,
            FileMutationCommand::Move {
                expected_revision: 2,
                destination_parent_id: Some(fixture.folder_id),
            },
        )
        .await
        .unwrap();
    assert_eq!(moved.parent_id, Some(fixture.folder_id));
    assert_eq!(moved.revision, 3);
    assert_eq!(
        std::fs::read(fixture.files().join("folder/ALPHA.TXT")).unwrap(),
        b"alpha"
    );

    let copied = source
        .mutate(
            fixture.project_id,
            fixture.file_id,
            FileMutationCommand::Copy {
                expected_revision: 3,
                destination_parent_id: None,
                name: FileExactName::parse("copy.txt").unwrap(),
            },
        )
        .await
        .unwrap();
    assert_ne!(copied.id, fixture.file_id);
    assert_eq!(copied.revision, 1);
    assert_eq!(
        std::fs::read(fixture.files().join("copy.txt")).unwrap(),
        b"alpha"
    );
    assert_ne!(copied.platform_identity, moved.platform_identity);
}

#[tokio::test]
async fn directory_entries_can_be_renamed_but_not_copied() {
    let fixture = Fixture::new().await;
    let source = ProductionFileMutationSource::open(fixture.pool.clone(), fixture.storage.clone())
        .await
        .unwrap();

    let renamed = source
        .mutate(
            fixture.project_id,
            fixture.folder_id,
            FileMutationCommand::Rename {
                expected_revision: 1,
                name: FileExactName::parse("archive").unwrap(),
            },
        )
        .await
        .unwrap();
    assert_eq!(renamed.kind, cellar_core::FileKind::Directory);
    assert_eq!(renamed.exact_name.as_str(), "archive");
    assert!(fixture.files().join("archive").is_dir());

    let copy = source
        .mutate(
            fixture.project_id,
            fixture.folder_id,
            FileMutationCommand::Copy {
                expected_revision: 2,
                destination_parent_id: None,
                name: FileExactName::parse("archive-copy").unwrap(),
            },
        )
        .await;
    assert_eq!(copy.unwrap_err(), FileMutationError::Unsupported);
}

#[tokio::test]
async fn settling_sources_and_recreated_destination_parents_are_rejected_before_mutation() {
    let fixture = Fixture::new().await;
    let source = ProductionFileMutationSource::open(fixture.pool.clone(), fixture.storage.clone())
        .await
        .unwrap();

    sqlx::query("UPDATE file_entry SET state = 'settling' WHERE id = ?")
        .bind(fixture.file_id.to_string())
        .execute(&fixture.pool)
        .await
        .unwrap();
    let settling = source
        .mutate(
            fixture.project_id,
            fixture.file_id,
            FileMutationCommand::Rename {
                expected_revision: 1,
                name: FileExactName::parse("blocked.txt").unwrap(),
            },
        )
        .await;
    assert_eq!(settling.unwrap_err(), FileMutationError::Unsupported);
    assert_eq!(
        std::fs::read(fixture.files().join("alpha.txt")).unwrap(),
        b"alpha"
    );

    sqlx::query("UPDATE file_entry SET state = 'live' WHERE id = ?")
        .bind(fixture.file_id.to_string())
        .execute(&fixture.pool)
        .await
        .unwrap();
    std::fs::remove_dir(fixture.files().join("folder")).unwrap();
    std::fs::create_dir(fixture.files().join("folder")).unwrap();
    let replaced_parent = source
        .mutate(
            fixture.project_id,
            fixture.file_id,
            FileMutationCommand::Move {
                expected_revision: 1,
                destination_parent_id: Some(fixture.folder_id),
            },
        )
        .await;
    assert_eq!(replaced_parent.unwrap_err(), FileMutationError::Conflict);
    assert_eq!(
        std::fs::read(fixture.files().join("alpha.txt")).unwrap(),
        b"alpha"
    );
}

#[tokio::test]
async fn a_live_destination_race_is_terminal_and_never_replayed() {
    let fixture = Fixture::new().await;
    let destination = fixture.files().join("race.txt");
    let reconciliation = Arc::new(RecordingReconciliation::default());
    let source = ProductionFileMutationSource::open_with_fault_injector_and_reconciliation(
        fixture.pool.clone(),
        fixture.storage.clone(),
        Arc::new(CreateDestinationAfterIntent {
            path: destination.clone(),
            fired: AtomicBool::new(false),
        }),
        reconciliation.clone(),
        Arc::new(InMemoryProjectMutationCoordinator::default()),
    )
    .await
    .unwrap();

    let conflict = source
        .mutate(
            fixture.project_id,
            fixture.file_id,
            FileMutationCommand::Rename {
                expected_revision: 1,
                name: FileExactName::parse("race.txt").unwrap(),
            },
        )
        .await;
    assert_eq!(conflict.unwrap_err(), FileMutationError::Conflict);
    assert_eq!(std::fs::read(&destination).unwrap(), b"external");
    std::fs::remove_file(&destination).unwrap();

    let recovery =
        ProductionFileMutationSource::open(fixture.pool.clone(), fixture.storage.clone())
            .await
            .unwrap();
    recovery.recover_pending().await.unwrap();

    assert!(!destination.exists());
    assert_eq!(
        std::fs::read(fixture.files().join("alpha.txt")).unwrap(),
        b"alpha"
    );
    let state: String = sqlx::query_scalar(
        "SELECT state FROM operation WHERE project_id = ? AND kind = 'file_rename'",
    )
    .bind(fixture.project_id.to_string())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(state, "failed");
    assert_eq!(
        *reconciliation.requests.lock().unwrap(),
        vec![ReconciliationRequest {
            project_id: fixture.project_id,
            file_id: fixture.file_id,
        }]
    );
}

struct FailOnceAt {
    point: FileMutationFaultPoint,
    fired: AtomicBool,
}

impl FileMutationFaultInjector for FailOnceAt {
    fn should_fail(&self, point: FileMutationFaultPoint) -> bool {
        point == self.point && !self.fired.swap(true, Ordering::SeqCst)
    }
}

struct CreateDestinationAfterIntent {
    path: std::path::PathBuf,
    fired: AtomicBool,
}

struct CreateDestinationAfterTemporaryRename {
    path: std::path::PathBuf,
    fired: AtomicBool,
}

struct AttemptParentEscapeAfterIntent {
    source: std::path::PathBuf,
    destination: std::path::PathBuf,
    escaped: Arc<Mutex<Option<bool>>>,
}

#[derive(Default)]
struct RecordingReconciliation {
    requests: Mutex<Vec<ReconciliationRequest>>,
}

impl ReconciliationScheduler for RecordingReconciliation {
    fn try_schedule(&self, request: ReconciliationRequest) -> bool {
        self.requests.lock().unwrap().push(request);
        true
    }
}

impl FileMutationFaultInjector for CreateDestinationAfterIntent {
    fn should_fail(&self, point: FileMutationFaultPoint) -> bool {
        if point == FileMutationFaultPoint::AfterIntent && !self.fired.swap(true, Ordering::SeqCst)
        {
            std::fs::write(&self.path, b"external").unwrap();
        }
        false
    }
}

impl FileMutationFaultInjector for CreateDestinationAfterTemporaryRename {
    fn should_fail(&self, point: FileMutationFaultPoint) -> bool {
        if point == FileMutationFaultPoint::AfterTemporaryRename
            && !self.fired.swap(true, Ordering::SeqCst)
        {
            std::fs::write(&self.path, b"external").unwrap();
        }
        false
    }
}

impl FileMutationFaultInjector for AttemptParentEscapeAfterIntent {
    fn should_fail(&self, point: FileMutationFaultPoint) -> bool {
        if point == FileMutationFaultPoint::AfterIntent {
            let escaped = std::fs::rename(&self.source, &self.destination).is_ok();
            *self.escaped.lock().unwrap() = Some(escaped);
        }
        false
    }
}

#[tokio::test]
async fn a_case_only_destination_race_returns_conflict_and_schedules_reconciliation() {
    let fixture = Fixture::new().await;
    let destination = fixture.files().join("ALPHA.TXT");
    let reconciliation = Arc::new(RecordingReconciliation::default());
    let source = ProductionFileMutationSource::open_with_fault_injector_and_reconciliation(
        fixture.pool.clone(),
        fixture.storage.clone(),
        Arc::new(CreateDestinationAfterTemporaryRename {
            path: destination.clone(),
            fired: AtomicBool::new(false),
        }),
        reconciliation.clone(),
        Arc::new(InMemoryProjectMutationCoordinator::default()),
    )
    .await
    .unwrap();

    let result = source
        .mutate(
            fixture.project_id,
            fixture.file_id,
            FileMutationCommand::Rename {
                expected_revision: 1,
                name: FileExactName::parse("ALPHA.TXT").unwrap(),
            },
        )
        .await;

    assert_eq!(result.unwrap_err(), FileMutationError::Conflict);
    assert_eq!(std::fs::read(&destination).unwrap(), b"external");
    assert!(
        exact_names(&fixture.files())
            .iter()
            .any(|name| name.starts_with(".cellar-case-"))
    );
    assert_eq!(
        *reconciliation.requests.lock().unwrap(),
        vec![ReconciliationRequest {
            project_id: fixture.project_id,
            file_id: fixture.file_id,
        }]
    );
}

#[tokio::test]
async fn destination_parent_cannot_escape_after_validation() {
    let fixture = Fixture::new().await;
    let outside = TempDir::new().unwrap();
    let escaped = Arc::new(Mutex::new(None));
    let source = ProductionFileMutationSource::open_with_fault_injector_and_reconciliation(
        fixture.pool.clone(),
        fixture.storage.clone(),
        Arc::new(AttemptParentEscapeAfterIntent {
            source: fixture.files().join("folder"),
            destination: outside.path().join("escaped"),
            escaped: escaped.clone(),
        }),
        Arc::new(RecordingReconciliation::default()),
        Arc::new(InMemoryProjectMutationCoordinator::default()),
    )
    .await
    .unwrap();

    let result = source
        .mutate(
            fixture.project_id,
            fixture.file_id,
            FileMutationCommand::Move {
                expected_revision: 1,
                destination_parent_id: Some(fixture.folder_id),
            },
        )
        .await;

    assert!(result.is_ok());
    assert_eq!(*escaped.lock().unwrap(), Some(false));
    assert!(!outside.path().join("escaped").exists());
    assert_eq!(
        std::fs::read(fixture.files().join("folder").join("alpha.txt")).unwrap(),
        b"alpha"
    );
}

#[tokio::test]
async fn copy_io_failure_removes_the_exact_partial_stage_before_terminalizing() {
    let fixture = Fixture::new().await;
    let reconciliation = Arc::new(RecordingReconciliation::default());
    let source = ProductionFileMutationSource::open_with_fault_injector_and_reconciliation(
        fixture.pool.clone(),
        fixture.storage.clone(),
        Arc::new(FailOnceAt {
            point: FileMutationFaultPoint::CopyIoFailure,
            fired: AtomicBool::new(false),
        }),
        reconciliation.clone(),
        Arc::new(InMemoryProjectMutationCoordinator::default()),
    )
    .await
    .unwrap();

    let result = source
        .mutate(
            fixture.project_id,
            fixture.file_id,
            FileMutationCommand::Copy {
                expected_revision: 1,
                destination_parent_id: None,
                name: FileExactName::parse("copy.txt").unwrap(),
            },
        )
        .await;

    assert_eq!(result.unwrap_err(), FileMutationError::InsufficientStorage);
    assert!(exact_names(&fixture.root.path().join(".cellar-file-mutation-staging")).is_empty());
    let state: String = sqlx::query_scalar(
        "SELECT state FROM operation WHERE project_id = ? AND kind = 'file_copy'",
    )
    .bind(fixture.project_id.to_string())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(state, "failed");
    assert_eq!(
        *reconciliation.requests.lock().unwrap(),
        vec![ReconciliationRequest {
            project_id: fixture.project_id,
            file_id: fixture.file_id,
        }]
    );
}

#[tokio::test]
async fn restart_completes_a_renamed_namespace_without_reapplying_it() {
    let fixture = Fixture::new().await;
    let faulting = ProductionFileMutationSource::open_with_fault_injector(
        fixture.pool.clone(),
        fixture.storage.clone(),
        Arc::new(FailOnceAt {
            point: FileMutationFaultPoint::AfterFilesystemApply,
            fired: AtomicBool::new(false),
        }),
    )
    .await
    .unwrap();

    let result = faulting
        .mutate(
            fixture.project_id,
            fixture.file_id,
            FileMutationCommand::Rename {
                expected_revision: 1,
                name: FileExactName::parse("renamed.txt").unwrap(),
            },
        )
        .await;
    assert_eq!(result.unwrap_err(), FileMutationError::Unavailable);
    assert!(!fixture.files().join("alpha.txt").exists());
    assert_eq!(
        std::fs::read(fixture.files().join("renamed.txt")).unwrap(),
        b"alpha"
    );

    let recovery =
        ProductionFileMutationSource::open(fixture.pool.clone(), fixture.storage.clone())
            .await
            .unwrap();
    recovery.recover_pending().await.unwrap();

    let row =
        sqlx::query("SELECT exact_name, revision FROM file_entry WHERE id = ? AND project_id = ?")
            .bind(fixture.file_id.to_string())
            .bind(fixture.project_id.to_string())
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    assert_eq!(row.get::<String, _>("exact_name"), "renamed.txt");
    assert_eq!(row.get::<i64, _>("revision"), 2);
    let state: String = sqlx::query_scalar(
        "SELECT state FROM operation WHERE project_id = ? AND kind = 'file_rename'",
    )
    .bind(fixture.project_id.to_string())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(state, "complete");
}

#[tokio::test]
async fn recovery_rejects_an_unexpected_object_recreated_at_the_old_source_name() {
    let fixture = Fixture::new().await;
    let faulting = ProductionFileMutationSource::open_with_fault_injector(
        fixture.pool.clone(),
        fixture.storage.clone(),
        Arc::new(FailOnceAt {
            point: FileMutationFaultPoint::AfterFilesystemApply,
            fired: AtomicBool::new(false),
        }),
    )
    .await
    .unwrap();

    let result = faulting
        .mutate(
            fixture.project_id,
            fixture.file_id,
            FileMutationCommand::Rename {
                expected_revision: 1,
                name: FileExactName::parse("renamed.txt").unwrap(),
            },
        )
        .await;
    assert_eq!(result.unwrap_err(), FileMutationError::Unavailable);
    std::fs::write(fixture.files().join("alpha.txt"), b"external").unwrap();

    let recovery =
        ProductionFileMutationSource::open(fixture.pool.clone(), fixture.storage.clone())
            .await
            .unwrap();
    recovery.recover_pending().await.unwrap();

    assert_eq!(
        std::fs::read(fixture.files().join("alpha.txt")).unwrap(),
        b"external"
    );
    assert_eq!(
        std::fs::read(fixture.files().join("renamed.txt")).unwrap(),
        b"alpha"
    );
    let row = sqlx::query("SELECT exact_name, revision FROM file_entry WHERE id = ?")
        .bind(fixture.file_id.to_string())
        .fetch_one(&fixture.pool)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("exact_name"), "alpha.txt");
    assert_eq!(row.get::<i64, _>("revision"), 1);
    let state: String = sqlx::query_scalar(
        "SELECT state FROM operation WHERE project_id = ? AND kind = 'file_rename'",
    )
    .bind(fixture.project_id.to_string())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(state, "failed");
}

#[tokio::test]
async fn recovery_preserves_both_entries_when_an_external_destination_wins() {
    let fixture = Fixture::new().await;
    let faulting = ProductionFileMutationSource::open_with_fault_injector(
        fixture.pool.clone(),
        fixture.storage.clone(),
        Arc::new(FailOnceAt {
            point: FileMutationFaultPoint::AfterIntent,
            fired: AtomicBool::new(false),
        }),
    )
    .await
    .unwrap();

    let result = faulting
        .mutate(
            fixture.project_id,
            fixture.file_id,
            FileMutationCommand::Rename {
                expected_revision: 1,
                name: FileExactName::parse("external-wins.txt").unwrap(),
            },
        )
        .await;
    assert_eq!(result.unwrap_err(), FileMutationError::Unavailable);
    std::fs::write(fixture.files().join("external-wins.txt"), b"external").unwrap();

    let recovery =
        ProductionFileMutationSource::open(fixture.pool.clone(), fixture.storage.clone())
            .await
            .unwrap();
    recovery.recover_pending().await.unwrap();

    assert_eq!(
        std::fs::read(fixture.files().join("alpha.txt")).unwrap(),
        b"alpha"
    );
    assert_eq!(
        std::fs::read(fixture.files().join("external-wins.txt")).unwrap(),
        b"external"
    );
    let row = sqlx::query("SELECT exact_name, revision FROM file_entry WHERE id = ?")
        .bind(fixture.file_id.to_string())
        .fetch_one(&fixture.pool)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("exact_name"), "alpha.txt");
    assert_eq!(row.get::<i64, _>("revision"), 1);
    let state: String = sqlx::query_scalar(
        "SELECT state FROM operation WHERE project_id = ? AND kind = 'file_rename'",
    )
    .bind(fixture.project_id.to_string())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(state, "failed");
}

#[tokio::test]
async fn restart_finishes_a_case_only_rename_from_its_temporary_name() {
    let fixture = Fixture::new().await;
    let faulting = ProductionFileMutationSource::open_with_fault_injector(
        fixture.pool.clone(),
        fixture.storage.clone(),
        Arc::new(FailOnceAt {
            point: FileMutationFaultPoint::AfterTemporaryRename,
            fired: AtomicBool::new(false),
        }),
    )
    .await
    .unwrap();

    let result = faulting
        .mutate(
            fixture.project_id,
            fixture.file_id,
            FileMutationCommand::Rename {
                expected_revision: 1,
                name: FileExactName::parse("ALPHA.TXT").unwrap(),
            },
        )
        .await;
    assert_eq!(result.unwrap_err(), FileMutationError::Unavailable);
    let interrupted_names = exact_names(&fixture.files());
    assert!(
        interrupted_names
            .iter()
            .any(|name| name.starts_with(".cellar-case-"))
    );

    let recovery =
        ProductionFileMutationSource::open(fixture.pool.clone(), fixture.storage.clone())
            .await
            .unwrap();
    recovery.recover_pending().await.unwrap();

    let names = exact_names(&fixture.files());
    assert!(names.iter().any(|name| name == "ALPHA.TXT"));
    assert!(!names.iter().any(|name| name.starts_with(".cellar-case-")));
    let row = sqlx::query("SELECT exact_name, revision FROM file_entry WHERE id = ?")
        .bind(fixture.file_id.to_string())
        .fetch_one(&fixture.pool)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("exact_name"), "ALPHA.TXT");
    assert_eq!(row.get::<i64, _>("revision"), 2);
}

#[tokio::test]
async fn restart_publishes_only_the_journaled_copy_staging_identity() {
    let fixture = Fixture::new().await;
    let faulting = ProductionFileMutationSource::open_with_fault_injector(
        fixture.pool.clone(),
        fixture.storage.clone(),
        Arc::new(FailOnceAt {
            point: FileMutationFaultPoint::AfterCopyStaging,
            fired: AtomicBool::new(false),
        }),
    )
    .await
    .unwrap();

    let result = faulting
        .mutate(
            fixture.project_id,
            fixture.file_id,
            FileMutationCommand::Copy {
                expected_revision: 1,
                destination_parent_id: None,
                name: FileExactName::parse("copy.txt").unwrap(),
            },
        )
        .await;
    assert_eq!(result.unwrap_err(), FileMutationError::Unavailable);
    assert!(!fixture.files().join("copy.txt").exists());

    let recovery =
        ProductionFileMutationSource::open(fixture.pool.clone(), fixture.storage.clone())
            .await
            .unwrap();
    recovery.recover_pending().await.unwrap();

    assert_eq!(
        std::fs::read(fixture.files().join("copy.txt")).unwrap(),
        b"alpha"
    );
    let copied: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM file_entry
         WHERE project_id = ? AND parent_id IS NULL AND exact_name = 'copy.txt'",
    )
    .bind(fixture.project_id.to_string())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(copied, 1);
}

#[tokio::test]
async fn restart_recopies_an_incomplete_hidden_staging_file_before_publication() {
    let fixture = Fixture::new().await;
    let faulting = ProductionFileMutationSource::open_with_fault_injector(
        fixture.pool.clone(),
        fixture.storage.clone(),
        Arc::new(FailOnceAt {
            point: FileMutationFaultPoint::AfterStagingIntent,
            fired: AtomicBool::new(false),
        }),
    )
    .await
    .unwrap();

    let result = faulting
        .mutate(
            fixture.project_id,
            fixture.file_id,
            FileMutationCommand::Copy {
                expected_revision: 1,
                destination_parent_id: None,
                name: FileExactName::parse("copy.txt").unwrap(),
            },
        )
        .await;
    assert_eq!(result.unwrap_err(), FileMutationError::Unavailable);
    assert!(!fixture.files().join("copy.txt").exists());

    let recovery =
        ProductionFileMutationSource::open(fixture.pool.clone(), fixture.storage.clone())
            .await
            .unwrap();
    recovery.recover_pending().await.unwrap();

    assert_eq!(
        std::fs::read(fixture.files().join("copy.txt")).unwrap(),
        b"alpha"
    );
    let state: String = sqlx::query_scalar(
        "SELECT state FROM operation WHERE project_id = ? AND kind = 'file_copy'",
    )
    .bind(fixture.project_id.to_string())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(state, "complete");
}

#[tokio::test]
async fn recovery_preserves_an_external_copy_destination_and_fails_the_operation() {
    let fixture = Fixture::new().await;
    let faulting = ProductionFileMutationSource::open_with_fault_injector(
        fixture.pool.clone(),
        fixture.storage.clone(),
        Arc::new(FailOnceAt {
            point: FileMutationFaultPoint::AfterCopyStaging,
            fired: AtomicBool::new(false),
        }),
    )
    .await
    .unwrap();

    let result = faulting
        .mutate(
            fixture.project_id,
            fixture.file_id,
            FileMutationCommand::Copy {
                expected_revision: 1,
                destination_parent_id: None,
                name: FileExactName::parse("copy.txt").unwrap(),
            },
        )
        .await;
    assert_eq!(result.unwrap_err(), FileMutationError::Unavailable);
    std::fs::write(fixture.files().join("copy.txt"), b"external").unwrap();

    let recovery =
        ProductionFileMutationSource::open(fixture.pool.clone(), fixture.storage.clone())
            .await
            .unwrap();
    recovery.recover_pending().await.unwrap();

    assert_eq!(
        std::fs::read(fixture.files().join("copy.txt")).unwrap(),
        b"external"
    );
    let copied: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM file_entry
         WHERE project_id = ? AND parent_id IS NULL AND exact_name = 'copy.txt'",
    )
    .bind(fixture.project_id.to_string())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(copied, 0);
    let state: String = sqlx::query_scalar(
        "SELECT state FROM operation WHERE project_id = ? AND kind = 'file_copy'",
    )
    .bind(fixture.project_id.to_string())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(state, "failed");
}
