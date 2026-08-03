mod common;

use std::{
    future::Future,
    io::{self, Write},
    pin::Pin,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::body::Body;
use cellar::{
    db::{Database, DbError, NewProject, ProjectRow},
    projects::{ProjectRepository, ProjectService, ProjectStorage},
    storage::StorageError,
};
use serde_json::{Value, json};
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};
use tokio::sync::{Semaphore, oneshot};
use tower::ServiceExt;
use uuid::{Uuid, Version};

use common::{
    ASSERTION_HEADER, TestContext, authenticated_request, authenticated_write_request,
    json_request, json_response,
};

const PROJECTS: &str = "/api/v1/projects";

#[tokio::test]
async fn authenticated_initial_list_is_empty_json_with_request_id() {
    let context = TestContext::new().await;
    let response = context
        .app()
        .oneshot(
            authenticated_request("GET", PROJECTS)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.headers()["content-type"], "application/json");
    let (status, request_id, body) = json_response(response).await;
    assert_eq!(status, 200);
    assert!(Uuid::parse_str(&request_id).is_ok());
    assert_eq!(body, json!([]));
    context.close().await;
}

#[tokio::test]
async fn valid_create_returns_exact_contract_and_uuid_directory() {
    let context = TestContext::new().await;
    let response = context
        .app()
        .oneshot(json_request(
            authenticated_write_request(PROJECTS),
            r#"{"name":"  Work  "}"#,
        ))
        .await
        .unwrap();
    let (status, _, body) = json_response(response).await;
    assert_eq!(status, 201);
    assert_eq!(body.as_object().unwrap().len(), 3);
    assert_eq!(body["name"], "Work");
    let id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();
    assert_eq!(id.get_version(), Some(Version::SortRand));
    let created_at = body["createdAt"].as_str().unwrap();
    let parsed = OffsetDateTime::parse(created_at, &Rfc3339).unwrap();
    assert_eq!(parsed.offset(), UtcOffset::UTC);
    assert_eq!(parsed.format(&Rfc3339).unwrap(), created_at);
    assert!(context.project_path(id).join("files").is_dir());
    context.close().await;
}

#[tokio::test]
async fn list_is_oldest_first_and_duplicate_names_are_allowed() {
    let context = TestContext::new().await;
    let app = context.app();
    let mut created = Vec::new();
    for name in ["Same", "Middle", "Same"] {
        let response = app
            .clone()
            .oneshot(json_request(
                authenticated_write_request(PROJECTS),
                &json!({"name": name}).to_string(),
            ))
            .await
            .unwrap();
        let (_, _, body) = json_response(response).await;
        created.push(body);
        tokio::task::yield_now().await;
    }
    let response = app
        .oneshot(
            authenticated_request("GET", PROJECTS)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, _, listed) = json_response(response).await;
    assert_eq!(status, 200);
    assert_eq!(listed, Value::Array(created));
    context.close().await;
}

#[tokio::test]
async fn security_boundary_handles_missing_and_invalid_assertions() {
    let context = TestContext::new().await;
    for request in [
        http::Request::get(PROJECTS).body(Body::empty()).unwrap(),
        http::Request::get(PROJECTS)
            .header(ASSERTION_HEADER, "invalid")
            .body(Body::empty())
            .unwrap(),
    ] {
        let (status, request_id, body) =
            json_response(context.app().oneshot(request).await.unwrap()).await;
        assert_eq!(status, 401);
        assert_eq!(body["error"]["code"], "unauthorized");
        assert_eq!(body["error"]["requestId"], request_id);
    }
    context.close().await;
}

#[tokio::test]
async fn missing_or_wrong_origin_is_rejected_before_creation() {
    let context = TestContext::new().await;
    for builder in [
        authenticated_request("POST", PROJECTS),
        authenticated_request("POST", PROJECTS).header("origin", "https://evil.example"),
    ] {
        let (status, _, _) = json_response(
            context
                .app()
                .oneshot(json_request(builder, r#"{"name":"Never"}"#))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, 403);
    }
    assert!(context.database.list_projects().await.unwrap().is_empty());
    assert_eq!(
        std::fs::read_dir(context.temp.path().join("projects"))
            .unwrap()
            .count(),
        0
    );
    context.close().await;
}

#[tokio::test]
async fn invalid_json_and_names_return_stable_400_without_side_effects() {
    let context = TestContext::new().await;
    let cases = [
        (Some("application/json"), r#"{}"#),
        (Some("application/json"), r#"{"name":""}"#),
        (Some("application/json"), r#"{"name":" \t\n "}"#),
        (
            Some("application/json"),
            &format!(r#"{{"name":"{}"}}"#, "a".repeat(101)),
        ),
        (Some("application/json"), r#"{"name":"bad\u0000name"}"#),
        (Some("application/json"), r#"{"name":1}"#),
        (Some("application/json"), r#"{"name":"Work","extra":true}"#),
        (Some("application/json"), r#"{"name":"broken""#),
        (Some("text/plain"), r#"{"name":"Work"}"#),
        (None, r#"{"name":"Work"}"#),
    ];
    for (content_type, body) in cases {
        let mut builder = authenticated_write_request(PROJECTS);
        if let Some(content_type) = content_type {
            builder = builder.header("content-type", content_type);
        }
        let response = context
            .app()
            .oneshot(builder.body(Body::from(body.to_owned())).unwrap())
            .await
            .unwrap();
        let (status, request_id, body) = json_response(response).await;
        assert_eq!(status, 400);
        assert_eq!(body["error"]["code"], "invalid_request");
        assert_eq!(body["error"]["message"], "The request is invalid.");
        assert_eq!(body["error"]["requestId"], request_id);
    }
    assert!(context.database.list_projects().await.unwrap().is_empty());
    assert_eq!(
        std::fs::read_dir(context.temp.path().join("projects"))
            .unwrap()
            .count(),
        0
    );
    context.close().await;
}

#[tokio::test]
async fn oversized_and_malformed_project_json_are_stable_400() {
    let context = TestContext::new().await;
    let oversized = json!({"name": "a".repeat(5_000)}).to_string();
    let response = context
        .app()
        .oneshot(json_request(
            authenticated_write_request(PROJECTS),
            &oversized,
        ))
        .await
        .unwrap();
    let (status, request_id, body) = json_response(response).await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "invalid_request");
    assert_eq!(body["error"]["requestId"], request_id);

    let malformed = context
        .app()
        .oneshot(json_request(
            authenticated_write_request(PROJECTS),
            r#"{"name":"broken""#,
        ))
        .await
        .unwrap();
    let (status, request_id, body) = json_response(malformed).await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "invalid_request");
    assert_eq!(body["error"]["requestId"], request_id);
    assert!(context.database.list_projects().await.unwrap().is_empty());
    assert_eq!(
        std::fs::read_dir(context.temp.path().join("projects"))
            .unwrap()
            .count(),
        0
    );
    context.close().await;
}

#[tokio::test]
async fn scalar_limit_applies_after_surrounding_unicode_whitespace_is_trimmed() {
    let context = TestContext::new().await;
    let accepted_name = format!("\u{2003}{}\u{2003}", "\u{1f600}".repeat(100));
    let accepted = context
        .app()
        .oneshot(json_request(
            authenticated_write_request(PROJECTS),
            &json!({"name": accepted_name}).to_string(),
        ))
        .await
        .unwrap();
    let (status, _, body) = json_response(accepted).await;
    assert_eq!(status, 201);
    assert_eq!(body["name"], "\u{1f600}".repeat(100));

    let rejected_name = format!("\u{2003}{}\u{2003}", "\u{1f600}".repeat(101));
    let rejected = context
        .app()
        .oneshot(json_request(
            authenticated_write_request(PROJECTS),
            &json!({"name": rejected_name}).to_string(),
        ))
        .await
        .unwrap();
    let (status, _, body) = json_response(rejected).await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "invalid_request");
    assert_eq!(context.database.list_projects().await.unwrap().len(), 1);
    context.close().await;
}

#[tokio::test]
async fn display_name_never_forms_any_filesystem_path() {
    let context = TestContext::new().await;
    let name = "visible project name";
    let response = context
        .app()
        .oneshot(json_request(
            authenticated_write_request(PROJECTS),
            &json!({"name": name}).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    let all_paths = walk_paths(context.temp.path());
    assert!(
        all_paths
            .iter()
            .all(|path| !path.to_string_lossy().contains(name))
    );
    context.close().await;
}

type RepoFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, DbError>> + Send + 'a>>;
type StorageFuture<'a> = Pin<Box<dyn Future<Output = Result<(), StorageError>> + Send + 'a>>;

#[derive(Clone)]
struct FailingRepository {
    database: Database,
    create_calls: Arc<AtomicUsize>,
}

impl ProjectRepository for FailingRepository {
    fn create_project<'a>(&'a self, _: NewProject) -> RepoFuture<'a, ProjectRow> {
        self.create_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(DbError::Conflict) })
    }

    fn list_projects<'a>(&'a self) -> RepoFuture<'a, Vec<ProjectRow>> {
        Box::pin(async move { self.database.list_projects().await })
    }
}

#[derive(Clone)]
struct FailingStorage {
    create_error: fn() -> StorageError,
    create_calls: Arc<AtomicUsize>,
    remove_calls: Arc<AtomicUsize>,
    cleanup_fails: bool,
}

impl ProjectStorage for FailingStorage {
    fn create_project_dir<'a>(&'a self, _: Uuid) -> StorageFuture<'a> {
        self.create_calls.fetch_add(1, Ordering::SeqCst);
        let error = (self.create_error)();
        Box::pin(async move { Err(error) })
    }

    fn remove_empty_project_dir<'a>(&'a self, _: Uuid) -> StorageFuture<'a> {
        self.remove_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if self.cleanup_fails {
                Err(StorageError::Io {
                    source: io::Error::new(io::ErrorKind::PermissionDenied, "C:\\secret\\name"),
                })
            } else {
                Ok(())
            }
        })
    }
}

#[tokio::test]
async fn directory_creation_failure_never_inserts_database_row() {
    let context = TestContext::new().await;
    let repo = FailingRepository {
        database: context.database.clone(),
        create_calls: Arc::new(AtomicUsize::new(0)),
    };
    let storage = FailingStorage {
        create_error: || StorageError::UnsafeManagedEntry,
        create_calls: Arc::new(AtomicUsize::new(0)),
        remove_calls: Arc::new(AtomicUsize::new(0)),
        cleanup_fails: false,
    };
    let create_calls = repo.create_calls.clone();
    let app = context.secure(ProjectService::new(Arc::new(repo), Arc::new(storage)));
    let response = app
        .oneshot(json_request(
            authenticated_write_request(PROJECTS),
            r#"{"name":"Work"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(create_calls.load(Ordering::SeqCst), 0);
    assert!(context.database.list_projects().await.unwrap().is_empty());
    context.close().await;
}

#[tokio::test]
async fn database_failure_removes_exact_created_directory_and_lists_nothing() {
    let context = TestContext::new().await;
    let sibling = context
        .temp
        .path()
        .join("projects")
        .join("unrelated-sibling");
    std::fs::create_dir(&sibling).unwrap();
    std::fs::write(sibling.join("keep.txt"), b"keep").unwrap();
    let repo = FailingRepository {
        database: context.database.clone(),
        create_calls: Arc::new(AtomicUsize::new(0)),
    };
    let app = context.secure(ProjectService::new(Arc::new(repo), context.storage.clone()));
    let response = app
        .clone()
        .oneshot(json_request(
            authenticated_write_request(PROJECTS),
            r#"{"name":"Secret name"}"#,
        ))
        .await
        .unwrap();
    let (status, _, body) = json_response(response).await;
    assert_eq!(status, 503);
    assert_eq!(body["error"]["code"], "project_create_failed");
    assert_eq!(std::fs::read(sibling.join("keep.txt")).unwrap(), b"keep");
    assert_eq!(
        std::fs::read_dir(context.temp.path().join("projects"))
            .unwrap()
            .count(),
        1
    );
    assert!(context.database.list_projects().await.unwrap().is_empty());
    let list_response = app
        .oneshot(
            authenticated_request("GET", PROJECTS)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, _, body) = json_response(list_response).await;
    assert_eq!(status, 200);
    assert_eq!(body, json!([]));
    context.close().await;
}

#[tokio::test]
async fn cleanup_failure_has_safe_classified_response_and_no_database_row() {
    let context = TestContext::new().await;
    let logs = captured_logs();
    let repo = FailingRepository {
        database: context.database.clone(),
        create_calls: Arc::new(AtomicUsize::new(0)),
    };
    let storage = Arc::new(CreatesThenFailsCleanup {
        root: context.temp.path().to_path_buf(),
    });
    let app = context.secure(ProjectService::new(Arc::new(repo), storage));
    let response = app
        .oneshot(json_request(
            authenticated_write_request(PROJECTS),
            r#"{"name":"Never log me"}"#,
        ))
        .await
        .unwrap();
    let (status, request_id, body) = json_response(response).await;
    assert_eq!(status, 503);
    assert_eq!(
        body,
        json!({"error": {
            "code": "project_cleanup_failed",
            "message": "Project creation could not be completed safely.",
            "requestId": request_id,
        }})
    );
    let rendered = body.to_string();
    assert!(!rendered.contains("Never log me"));
    assert!(!rendered.contains(context.temp.path().to_string_lossy().as_ref()));
    assert!(context.database.list_projects().await.unwrap().is_empty());
    let project_id = std::fs::read_dir(context.temp.path().join("projects"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name()
        .to_string_lossy()
        .into_owned();
    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("directory_cleanup_failed"));
    assert!(logs.contains(&request_id));
    assert!(logs.contains(&project_id));
    for secret in [
        "Never log me",
        context.temp.path().to_string_lossy().as_ref(),
        r"D:\private\project",
        "database constraint conflict",
    ] {
        assert!(!logs.contains(secret), "logs contained secret {secret}");
    }
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

fn captured_logs() -> Arc<Mutex<Vec<u8>>> {
    CAPTURED_LOGS
        .get_or_init(|| {
            let logs = Arc::new(Mutex::new(Vec::new()));
            let log_writer = CapturedWriter(logs.clone());
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(move || log_writer.clone())
                .finish();
            tracing::subscriber::set_global_default(subscriber).unwrap();
            logs
        })
        .clone()
}

#[derive(Clone)]
struct ListFailingRepository {
    database: Database,
}

impl ProjectRepository for ListFailingRepository {
    fn create_project<'a>(&'a self, project: NewProject) -> RepoFuture<'a, ProjectRow> {
        Box::pin(async move { self.database.create_project(project).await })
    }

    fn list_projects<'a>(&'a self) -> RepoFuture<'a, Vec<ProjectRow>> {
        Box::pin(async { Err(DbError::CorruptData) })
    }
}

#[tokio::test]
async fn list_database_failure_logs_only_closed_reason_and_request_id() {
    let context = TestContext::new().await;
    let logs = captured_logs();
    let app = context.secure(ProjectService::new(
        Arc::new(ListFailingRepository {
            database: context.database.clone(),
        }),
        context.storage.clone(),
    ));
    let response = app
        .oneshot(
            authenticated_request("GET", PROJECTS)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, request_id, body) = json_response(response).await;
    assert_eq!(status, 503);
    assert_eq!(body["error"]["code"], "project_list_failed");
    assert_eq!(body["error"]["requestId"], request_id);
    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    assert!(
        logs.lines()
            .any(|line| line.contains(&request_id) && line.contains("database_list_failed"))
    );
    assert!(!logs.contains("database contains invalid data"));
    context.close().await;
}

struct CreatesThenFailsCleanup {
    root: std::path::PathBuf,
}

impl ProjectStorage for CreatesThenFailsCleanup {
    fn create_project_dir<'a>(&'a self, id: Uuid) -> StorageFuture<'a> {
        Box::pin(async move {
            tokio::fs::create_dir(self.root.join("projects").join(id.to_string()))
                .await
                .unwrap();
            tokio::fs::create_dir(
                self.root
                    .join("projects")
                    .join(id.to_string())
                    .join("files"),
            )
            .await
            .unwrap();
            Ok(())
        })
    }

    fn remove_empty_project_dir<'a>(&'a self, _: Uuid) -> StorageFuture<'a> {
        Box::pin(async {
            Err(StorageError::Io {
                source: io::Error::new(io::ErrorKind::PermissionDenied, "D:\\private\\project"),
            })
        })
    }
}

#[tokio::test]
async fn insufficient_storage_maps_to_507() {
    let context = TestContext::new().await;
    let repo = FailingRepository {
        database: context.database.clone(),
        create_calls: Arc::new(AtomicUsize::new(0)),
    };
    let storage = FailingStorage {
        create_error: || StorageError::InsufficientSpace,
        create_calls: Arc::new(AtomicUsize::new(0)),
        remove_calls: Arc::new(AtomicUsize::new(0)),
        cleanup_fails: false,
    };
    let response = context
        .secure(ProjectService::new(Arc::new(repo), Arc::new(storage)))
        .oneshot(json_request(
            authenticated_write_request(PROJECTS),
            r#"{"name":"Work"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 507);
    context.close().await;
}

#[tokio::test]
async fn storage_internal_project_cleanup_failure_maps_to_cleanup_failed() {
    let context = TestContext::new().await;
    let logs = captured_logs();
    let repo = FailingRepository {
        database: context.database.clone(),
        create_calls: Arc::new(AtomicUsize::new(0)),
    };
    let create_calls = repo.create_calls.clone();
    let storage = FailingStorage {
        create_error: || StorageError::ProjectCleanupFailed {
            source: io::Error::new(io::ErrorKind::PermissionDenied, r"D:\private\project"),
        },
        create_calls: Arc::new(AtomicUsize::new(0)),
        remove_calls: Arc::new(AtomicUsize::new(0)),
        cleanup_fails: false,
    };
    let response = context
        .secure(ProjectService::new(Arc::new(repo), Arc::new(storage)))
        .oneshot(json_request(
            authenticated_write_request(PROJECTS),
            r#"{"name":"Work"}"#,
        ))
        .await
        .unwrap();
    let (status, request_id, body) = json_response(response).await;
    assert_eq!(status, 503);
    assert_eq!(body["error"]["code"], "project_cleanup_failed");
    assert_eq!(body["error"]["requestId"], request_id);
    assert_eq!(create_calls.load(Ordering::SeqCst), 0);
    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    assert!(
        logs.lines()
            .any(|line| line.contains(&request_id) && line.contains("directory_cleanup_failed"))
    );
    assert!(!logs.contains(r"D:\private\project"));
    context.close().await;
}

struct PausingStorage {
    storage: Arc<cellar::storage::Storage>,
    created: Mutex<Option<oneshot::Sender<Uuid>>>,
    release: Arc<Semaphore>,
}

impl ProjectStorage for PausingStorage {
    fn create_project_dir<'a>(&'a self, id: Uuid) -> StorageFuture<'a> {
        let created = self.created.lock().unwrap().take();
        Box::pin(async move {
            self.storage.create_project_dir(id).await?;
            if let Some(created) = created {
                let _ = created.send(id);
            }
            self.release.acquire().await.unwrap().forget();
            Ok(())
        })
    }

    fn remove_empty_project_dir<'a>(&'a self, id: Uuid) -> StorageFuture<'a> {
        Box::pin(async move { self.storage.remove_empty_project_dir(id).await })
    }
}

#[tokio::test]
async fn abort_after_directory_creation_finishes_insert_or_compensation() {
    let context = TestContext::new().await;
    let (created_tx, created_rx) = oneshot::channel();
    let release = Arc::new(Semaphore::new(0));
    let storage = Arc::new(PausingStorage {
        storage: context.storage.clone(),
        created: Mutex::new(Some(created_tx)),
        release: release.clone(),
    });
    let app = context.secure(ProjectService::new(
        Arc::new(context.database.clone()),
        storage,
    ));
    let request = tokio::spawn(app.oneshot(json_request(
        authenticated_write_request(PROJECTS),
        r#"{"name":"Cancellation boundary"}"#,
    )));
    let project_id = tokio::time::timeout(std::time::Duration::from_secs(2), created_rx)
        .await
        .unwrap()
        .unwrap();
    assert!(context.project_path(project_id).join("files").is_dir());

    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    release.add_permits(1);

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let rows = context.database.list_projects().await.unwrap();
            let directory_exists = context.project_path(project_id).exists();
            if (rows.iter().any(|row| row.id() == project_id) && directory_exists)
                || (rows.is_empty() && !directory_exists)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owned create operation did not finish insert or compensation");
    context.close().await;
}

#[tokio::test]
async fn concurrent_creates_have_distinct_v7_ids_complete_directories_and_rows() {
    let context = TestContext::new().await;
    let app = context.app();
    let requests = (0..24).map(|index| {
        app.clone().oneshot(json_request(
            authenticated_write_request(PROJECTS),
            &json!({"name": format!("Project {index}")}).to_string(),
        ))
    });
    let mut ids = Vec::new();
    for response in futures_util::future::join_all(requests).await {
        let (status, _, body) = json_response(response.unwrap()).await;
        assert_eq!(status, 201);
        let id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();
        assert_eq!(id.get_version(), Some(Version::SortRand));
        assert!(context.project_path(id).join("files").is_dir());
        ids.push(id);
    }
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 24);
    assert_eq!(context.database.list_projects().await.unwrap().len(), 24);
    context.close().await;
}

fn walk_paths(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                pending.push(path.clone());
            }
            found.push(path);
        }
    }
    found
}
