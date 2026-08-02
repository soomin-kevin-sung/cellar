use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{HeaderName, Method, Request, StatusCode, header};
use cellar_api::routes::files::{
    DownloadError, DownloadSource, FileMutationCommand, FileMutationError, FileMutationSource,
    VerifiedDownload, files_router_with_services,
};
use cellar_api::routes::session::session_router_with_routes;
use cellar_auth::{
    AccessClaims, CompareAndSet, CsrfManager, EnrollmentService, EnrollmentSnapshot,
    EnrollmentStore, EnrollmentStoreError,
};
use cellar_core::{
    FileEntry, FileEntryId, FileExactName, FileHashState, FileKind, FileListRequest, FilePage,
    FileRepository, FileRepositoryError, FileService, FileState, PlatformIdentity, ProjectId,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use time::OffsetDateTime;
use tower::ServiceExt;

const NOW: i64 = 50_000;
const ORIGIN: &str = "https://cellar.example";
const SUBJECT: &str = "owner-subject";
const REQUEST_ID: &str = "mutation-request";

struct EnrolledStore;

impl EnrollmentStore for EnrolledStore {
    fn load(&self) -> Result<EnrollmentSnapshot, EnrollmentStoreError> {
        Ok(EnrollmentSnapshot::enrolled(ORIGIN, SUBJECT))
    }

    fn compare_and_set_owner(
        &self,
        _: &EnrollmentSnapshot,
        _: &str,
    ) -> Result<CompareAndSet, EnrollmentStoreError> {
        Ok(CompareAndSet::Changed)
    }
}

struct EmptyRepository;

#[async_trait]
impl FileRepository for EmptyRepository {
    async fn list(&self, _: FileListRequest) -> Result<FilePage, FileRepositoryError> {
        Err(FileRepositoryError::Unavailable)
    }
}

struct DisabledDownloads;

#[async_trait]
impl DownloadSource for DisabledDownloads {
    async fn open_verified(
        &self,
        _: ProjectId,
        _: FileEntryId,
    ) -> Result<Box<dyn VerifiedDownload>, DownloadError> {
        Err(DownloadError::Unavailable)
    }
}

#[derive(Clone)]
struct MemoryMutations {
    calls: Arc<Mutex<Vec<(ProjectId, FileEntryId, FileMutationCommand)>>>,
    outcome: Result<FileEntry, FileMutationError>,
}

impl MemoryMutations {
    fn succeeds(entry: FileEntry) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            outcome: Ok(entry),
        }
    }

    fn fails(error: FileMutationError) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            outcome: Err(error),
        }
    }
}

#[async_trait]
impl FileMutationSource for MemoryMutations {
    async fn mutate(
        &self,
        project_id: ProjectId,
        file_id: FileEntryId,
        command: FileMutationCommand,
    ) -> Result<FileEntry, FileMutationError> {
        self.calls
            .lock()
            .unwrap()
            .push((project_id, file_id, command));
        self.outcome.clone()
    }
}

fn claims() -> AccessClaims {
    AccessClaims {
        iss: "https://issuer.example".into(),
        aud: vec!["aud".into()],
        sub: SUBJECT.into(),
        email: Some("owner@example.com".into()),
        exp: NOW + 40_000,
        nbf: NOW,
        iat: NOW,
        r#type: "app".into(),
    }
}

fn entry(project_id: ProjectId, file_id: FileEntryId, name: &str, revision: i64) -> FileEntry {
    FileEntry {
        id: file_id,
        project_id,
        parent_id: None,
        exact_name: FileExactName::parse(name).unwrap(),
        relative_path: name.to_owned(),
        kind: FileKind::File,
        platform_identity: PlatformIdentity::try_new("ntfs", Some(vec![1; 8]), Some(vec![2; 16]))
            .unwrap(),
        size: 3,
        mtime_filetime_100ns: 1,
        hash: None,
        hash_state: FileHashState::Unknown,
        state: FileState::Live,
        revision,
        scan_generation: 0,
        observed_at: OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
    }
}

fn app(mutations: impl FileMutationSource + 'static) -> axum::Router {
    let protected = files_router_with_services::<EnrolledStore>(
        FileService::new(Arc::new(EmptyRepository)),
        Arc::new(DisabledDownloads),
        Arc::new(mutations),
    );
    session_router_with_routes(
        EnrollmentService::new(Arc::new(EnrolledStore)),
        Arc::new(CsrfManager::new()),
        || NOW,
        protected,
    )
}

async fn csrf(router: &axum::Router) -> String {
    let mut request = Request::builder()
        .uri("/api/v1/session")
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(claims());
    json_body(router.clone().oneshot(request).await.unwrap()).await["csrf_token"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn mutate(
    router: axum::Router,
    project_id: ProjectId,
    file_id: FileEntryId,
    action: &str,
    body: Value,
) -> axum::response::Response {
    let csrf = csrf(&router).await;
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(format!(
            "/api/v1/projects/{project_id}/files/{file_id}/{action}"
        ))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ORIGIN, ORIGIN)
        .header("x-request-id", REQUEST_ID)
        .header(HeaderName::from_static("x-cellar-csrf"), csrf)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    request.extensions_mut().insert(claims());
    router.oneshot(request).await.unwrap()
}

async fn json_body(response: axum::response::Response) -> Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[tokio::test]
async fn rename_move_and_copy_use_exact_versioned_commands() {
    let project_id = ProjectId::new();
    let file_id = FileEntryId::new();
    let destination = FileEntryId::new();
    let mutations = MemoryMutations::succeeds(entry(project_id, file_id, "renamed.txt", 8));
    let calls = Arc::clone(&mutations.calls);
    let router = app(mutations);

    let rename = mutate(
        router.clone(),
        project_id,
        file_id,
        "rename",
        json!({"expectedRevision":"7", "name":"renamed.txt"}),
    )
    .await;
    assert_eq!(rename.status(), StatusCode::OK);
    assert_eq!(json_body(rename).await["exactName"], "renamed.txt");

    let moved = mutate(
        router.clone(),
        project_id,
        file_id,
        "move",
        json!({"expectedRevision":"7", "destinationParentId":destination.to_string()}),
    )
    .await;
    assert_eq!(moved.status(), StatusCode::OK);

    let copied = mutate(
        router,
        project_id,
        file_id,
        "copy",
        json!({
            "expectedRevision":"7",
            "destinationParentId":null,
            "name":"copy.txt"
        }),
    )
    .await;
    assert_eq!(copied.status(), StatusCode::OK);

    assert_eq!(
        *calls.lock().unwrap(),
        vec![
            (
                project_id,
                file_id,
                FileMutationCommand::Rename {
                    expected_revision: 7,
                    name: FileExactName::parse("renamed.txt").unwrap(),
                },
            ),
            (
                project_id,
                file_id,
                FileMutationCommand::Move {
                    expected_revision: 7,
                    destination_parent_id: Some(destination),
                },
            ),
            (
                project_id,
                file_id,
                FileMutationCommand::Copy {
                    expected_revision: 7,
                    destination_parent_id: None,
                    name: FileExactName::parse("copy.txt").unwrap(),
                },
            ),
        ]
    );
}

#[tokio::test]
async fn destination_conflict_is_preserved_as_409() {
    let project_id = ProjectId::new();
    let file_id = FileEntryId::new();
    let response = mutate(
        app(MemoryMutations::fails(FileMutationError::Conflict)),
        project_id,
        file_id,
        "rename",
        json!({"expectedRevision":"2", "name":"occupied.txt"}),
    )
    .await;

    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        json_body(response).await["code"],
        "file_destination_conflict"
    );
}

#[tokio::test]
async fn malformed_names_versions_and_payloads_never_reach_storage() {
    for (action, body) in [
        (
            "rename",
            json!({"expectedRevision":"01", "name":"next.txt"}),
        ),
        (
            "rename",
            json!({"expectedRevision":"1", "name":"../escape"}),
        ),
        (
            "move",
            json!({"expectedRevision":"0", "destinationParentId":null}),
        ),
        (
            "copy",
            json!({"expectedRevision":"1", "destinationParentId":null}),
        ),
        ("unknown", json!({"expectedRevision":"1"})),
    ] {
        let project_id = ProjectId::new();
        let file_id = FileEntryId::new();
        let mutations = MemoryMutations::succeeds(entry(project_id, file_id, "ok.txt", 2));
        let calls = Arc::clone(&mutations.calls);
        let response = mutate(app(mutations), project_id, file_id, action, body).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{action}");
        assert!(calls.lock().unwrap().is_empty(), "{action}");
    }
}
