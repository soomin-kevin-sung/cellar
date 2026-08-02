use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{HeaderName, Method, Request, StatusCode, header};
use cellar_api::routes::files::{
    DOWNLOAD_CHUNK_BYTES, DownloadError, DownloadMetadata, DownloadReadError, DownloadReadLease,
    DownloadSource, DownloadSpan, MAX_OPEN_DOWNLOADS, VerifiedDownload,
    files_router_with_downloads,
};
use cellar_api::routes::session::session_router_with_routes;
use cellar_auth::{
    AccessClaims, CompareAndSet, CsrfManager, EnrollmentService, EnrollmentSnapshot,
    EnrollmentStore, EnrollmentStoreError,
};
use cellar_core::{
    FileEntryId, FileListRequest, FilePage, FileRepository, FileRepositoryError, FileService,
    ProjectId,
};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

const NOW: i64 = 50_000;
const ORIGIN: &str = "https://cellar.example";
const SUBJECT: &str = "owner-subject";
const REQUEST_ID: &str = "range-request";

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

#[derive(Clone)]
struct MemorySource {
    outcome: Arc<Mutex<VecDeque<Result<MemoryDownload, DownloadError>>>>,
}

impl MemorySource {
    fn ready(filename: &str, bytes: Vec<u8>, sha256: Option<[u8; 32]>) -> Self {
        Self::outcomes([Ok(MemoryDownload {
            metadata: DownloadMetadata::new(filename, bytes.len() as u64, sha256).unwrap(),
            bytes,
            verify_error: None,
        })])
    }

    fn identity_changed(filename: &str, bytes: Vec<u8>) -> Self {
        Self::outcomes([Ok(MemoryDownload {
            metadata: DownloadMetadata::new(filename, bytes.len() as u64, None).unwrap(),
            bytes,
            verify_error: Some(DownloadError::IdentityChanged),
        })])
    }

    fn error(error: DownloadError) -> Self {
        Self::outcomes([Err(error)])
    }

    fn outcomes(outcomes: impl IntoIterator<Item = Result<MemoryDownload, DownloadError>>) -> Self {
        Self {
            outcome: Arc::new(Mutex::new(outcomes.into_iter().collect())),
        }
    }
}

#[async_trait]
impl DownloadSource for MemorySource {
    async fn open_verified(
        &self,
        _: ProjectId,
        _: FileEntryId,
    ) -> Result<Box<dyn VerifiedDownload>, DownloadError> {
        self.outcome
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Err(DownloadError::Unavailable))
            .map(|download| Box::new(download) as Box<dyn VerifiedDownload>)
    }
}

struct MemoryDownload {
    metadata: DownloadMetadata,
    bytes: Vec<u8>,
    verify_error: Option<DownloadError>,
}

#[async_trait]
impl VerifiedDownload for MemoryDownload {
    fn metadata(&self) -> &DownloadMetadata {
        &self.metadata
    }

    async fn verify(&self) -> Result<(), DownloadError> {
        self.verify_error.map_or(Ok(()), Err)
    }

    async fn read_exact_chunk(
        &mut self,
        span: DownloadSpan,
        _lease: DownloadReadLease,
    ) -> Result<Vec<u8>, DownloadReadError> {
        let start = span.start() as usize;
        let end = start + span.length() as usize;
        Ok(self.bytes[start..end].to_vec())
    }
}

#[derive(Clone, Copy)]
enum ProbeBehavior {
    Normal,
    ShortFirst,
    ErrorAfterFirst,
}

#[derive(Clone)]
struct ProbeSource {
    bytes: Arc<Vec<u8>>,
    behavior: ProbeBehavior,
    opens: Arc<AtomicUsize>,
    reads: Arc<AtomicUsize>,
    max_request: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}

impl ProbeSource {
    fn new(bytes: Vec<u8>, behavior: ProbeBehavior) -> Self {
        Self {
            bytes: Arc::new(bytes),
            behavior,
            opens: Arc::new(AtomicUsize::new(0)),
            reads: Arc::new(AtomicUsize::new(0)),
            max_request: Arc::new(AtomicUsize::new(0)),
            drops: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl DownloadSource for ProbeSource {
    async fn open_verified(
        &self,
        _: ProjectId,
        _: FileEntryId,
    ) -> Result<Box<dyn VerifiedDownload>, DownloadError> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(ProbeDownload {
            metadata: DownloadMetadata::new("large.bin", self.bytes.len() as u64, None).unwrap(),
            bytes: Arc::clone(&self.bytes),
            behavior: self.behavior,
            local_reads: 0,
            reads: Arc::clone(&self.reads),
            max_request: Arc::clone(&self.max_request),
            drops: Arc::clone(&self.drops),
        }))
    }
}

struct ProbeDownload {
    metadata: DownloadMetadata,
    bytes: Arc<Vec<u8>>,
    behavior: ProbeBehavior,
    local_reads: usize,
    reads: Arc<AtomicUsize>,
    max_request: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}

#[derive(Clone, Default)]
struct BlockingReadSource {
    state: Arc<BlockingReadState>,
}

#[derive(Default)]
struct BlockingReadState {
    gate: (Mutex<bool>, Condvar),
    started: AtomicUsize,
    completed: AtomicUsize,
    handles: Mutex<Vec<Weak<()>>>,
}

impl BlockingReadSource {
    async fn wait_for(&self, counter: &AtomicUsize, expected: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while counter.load(Ordering::SeqCst) != expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("blocking download workers reached the expected state");
    }

    fn release(&self) {
        let (released, condition) = &self.state.gate;
        *released.lock().unwrap() = true;
        condition.notify_all();
    }

    fn every_handle_is_retained(&self) -> bool {
        let handles = self.state.handles.lock().unwrap();
        handles.len() == MAX_OPEN_DOWNLOADS
            && handles.iter().all(|handle| handle.upgrade().is_some())
    }

    fn every_handle_is_released(&self) -> bool {
        self.state
            .handles
            .lock()
            .unwrap()
            .iter()
            .all(|handle| handle.upgrade().is_none())
    }
}

#[async_trait]
impl DownloadSource for BlockingReadSource {
    async fn open_verified(
        &self,
        _: ProjectId,
        _: FileEntryId,
    ) -> Result<Box<dyn VerifiedDownload>, DownloadError> {
        let handle = Arc::new(());
        self.state
            .handles
            .lock()
            .unwrap()
            .push(Arc::downgrade(&handle));
        Ok(Box::new(BlockingReadDownload {
            metadata: DownloadMetadata::new("blocked.bin", 1, None).unwrap(),
            state: Arc::clone(&self.state),
            handle,
        }))
    }
}

struct BlockingReadDownload {
    metadata: DownloadMetadata,
    state: Arc<BlockingReadState>,
    handle: Arc<()>,
}

#[async_trait]
impl VerifiedDownload for BlockingReadDownload {
    fn metadata(&self) -> &DownloadMetadata {
        &self.metadata
    }

    async fn verify(&self) -> Result<(), DownloadError> {
        Ok(())
    }

    async fn read_exact_chunk(
        &mut self,
        _: DownloadSpan,
        lease: DownloadReadLease,
    ) -> Result<Vec<u8>, DownloadReadError> {
        let state = Arc::clone(&self.state);
        let handle = Arc::clone(&self.handle);
        tokio::task::spawn_blocking(move || {
            state.started.fetch_add(1, Ordering::SeqCst);
            let (released, condition) = &state.gate;
            let mut released = released.lock().unwrap();
            while !*released {
                released = condition.wait(released).unwrap();
            }
            drop(released);
            drop(handle);
            drop(lease);
            state.completed.fetch_add(1, Ordering::SeqCst);
            vec![0x5a]
        })
        .await
        .map_err(|_| DownloadReadError::Io)
    }
}

impl Drop for ProbeDownload {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl VerifiedDownload for ProbeDownload {
    fn metadata(&self) -> &DownloadMetadata {
        &self.metadata
    }

    async fn verify(&self) -> Result<(), DownloadError> {
        Ok(())
    }

    async fn read_exact_chunk(
        &mut self,
        span: DownloadSpan,
        _lease: DownloadReadLease,
    ) -> Result<Vec<u8>, DownloadReadError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.max_request
            .fetch_max(span.length() as usize, Ordering::SeqCst);
        if matches!(self.behavior, ProbeBehavior::ErrorAfterFirst) && self.local_reads > 0 {
            return Err(DownloadReadError::Io);
        }
        self.local_reads += 1;
        let start = span.start() as usize;
        let mut end = start + span.length() as usize;
        if matches!(self.behavior, ProbeBehavior::ShortFirst) && self.local_reads == 1 {
            end -= 1;
        }
        Ok(self.bytes[start..end].to_vec())
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

fn app(source: impl DownloadSource + 'static) -> axum::Router {
    let service = FileService::new(Arc::new(EmptyRepository));
    let protected = files_router_with_downloads::<EnrolledStore>(service, Arc::new(source));
    session_router_with_routes(
        EnrollmentService::new(Arc::new(EnrolledStore)),
        Arc::new(CsrfManager::new()),
        || NOW,
        protected,
    )
}

fn download_request(
    project: ProjectId,
    file: FileEntryId,
    method: Method,
    range: Option<&str>,
    if_range: Option<&str>,
    authenticated: bool,
) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(format!("/api/v1/projects/{project}/files/{file}/download"))
        .header("x-request-id", REQUEST_ID)
        .body(Body::empty())
        .unwrap();
    if let Some(value) = range {
        request
            .headers_mut()
            .insert(header::RANGE, value.parse().unwrap());
    }
    if let Some(value) = if_range {
        request
            .headers_mut()
            .insert(header::IF_RANGE, value.parse().unwrap());
    }
    if authenticated {
        request.extensions_mut().insert(claims());
    }
    request
}

async fn bytes(response: axum::response::Response) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

async fn json(response: axum::response::Response) -> Value {
    serde_json::from_slice(&bytes(response).await).unwrap()
}

#[tokio::test]
async fn single_range_matrix_is_exact_and_malformed_or_multiple_is_ignored() {
    let project = ProjectId::new();
    let file = FileEntryId::new();
    let content: Vec<u8> = (0_u8..100).collect();
    for (range, status, content_range, expected) in [
        (
            Some("bytes=0-9"),
            206,
            Some("bytes 0-9/100"),
            content[0..10].to_vec(),
        ),
        (
            Some("bytes=90-"),
            206,
            Some("bytes 90-99/100"),
            content[90..].to_vec(),
        ),
        (
            Some("bytes=-10"),
            206,
            Some("bytes 90-99/100"),
            content[90..].to_vec(),
        ),
        (
            Some("bytes=0-999"),
            206,
            Some("bytes 0-99/100"),
            content.clone(),
        ),
        (
            Some("bytes=0-9999999999999999999999999999999999999999"),
            206,
            Some("bytes 0-99/100"),
            content.clone(),
        ),
        (
            Some("bytes=-9999999999999999999999999999999999999999"),
            206,
            Some("bytes 0-99/100"),
            content.clone(),
        ),
        (None, 200, None, content.clone()),
        (Some("garbage"), 200, None, content.clone()),
        (Some("bytes=0-1,4-5"), 200, None, content.clone()),
        (Some("bytes=x-y"), 200, None, content.clone()),
        (Some("items=0-1"), 200, None, content.clone()),
    ] {
        let response = app(MemorySource::ready("payload.bin", content.clone(), None))
            .oneshot(download_request(
                project,
                file,
                Method::GET,
                range,
                None,
                true,
            ))
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), status, "range {range:?}");
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_RANGE)
                .map(|value| value.to_str().unwrap()),
            content_range,
            "range {range:?}"
        );
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH],
            expected.len().to_string()
        );
        assert_eq!(bytes(response).await, expected, "range {range:?}");
    }
}

#[tokio::test]
async fn unsatisfiable_and_empty_ranges_return_416_with_stable_envelope() {
    let project = ProjectId::new();
    let file = FileEntryId::new();
    for (content, range, expected_content_range) in [
        (vec![0_u8; 100], "bytes=100-", "bytes */100"),
        (vec![0_u8; 100], "bytes=-0", "bytes */100"),
        (vec![0_u8; 100], "bytes=9-3", "bytes */100"),
        (Vec::new(), "bytes=0-", "bytes */0"),
        (Vec::new(), "bytes=-1", "bytes */0"),
        (
            vec![0_u8; 100],
            "bytes=9999999999999999999999999999999999999999-",
            "bytes */100",
        ),
    ] {
        let response = app(MemorySource::ready("payload.bin", content, None))
            .oneshot(download_request(
                project,
                file,
                Method::GET,
                Some(range),
                None,
                true,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(
            response.headers()[header::CONTENT_RANGE],
            expected_content_range
        );
        assert_eq!(response.headers()["x-request-id"], REQUEST_ID);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, no-store"
        );
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(
            response.headers()["cross-origin-resource-policy"],
            "same-origin"
        );
        let body = json(response).await;
        assert_eq!(body["code"], "invalid_range");
        assert_eq!(body["requestId"], REQUEST_ID);
        assert_eq!(body["details"], serde_json::json!({}));
    }
}

#[tokio::test]
async fn head_matches_get_headers_without_a_body_and_if_range_controls_resume() {
    let project = ProjectId::new();
    let file = FileEntryId::new();
    let content: Vec<u8> = (0_u8..100).collect();
    let expected_etag = "\"sha256-q6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6s\"";

    for (range, if_range, expected_status, expected_range, expected_len) in [
        (
            Some("bytes=5-9"),
            Some(expected_etag),
            206,
            Some("bytes 5-9/100"),
            "5",
        ),
        (Some("bytes=5-9"), Some("\"different\""), 200, None, "100"),
        (Some("bytes=100-"), Some("\"different\""), 200, None, "100"),
        (
            Some("bytes=5-9"),
            Some("W/\"sha256-q6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6s\""),
            200,
            None,
            "100",
        ),
    ] {
        let source = || MemorySource::ready("payload.bin", content.clone(), Some([0xab; 32]));
        let get = app(source())
            .oneshot(download_request(
                project,
                file,
                Method::GET,
                range,
                if_range,
                true,
            ))
            .await
            .unwrap();
        let head = app(source())
            .oneshot(download_request(
                project,
                file,
                Method::HEAD,
                range,
                if_range,
                true,
            ))
            .await
            .unwrap();
        assert_eq!(get.status().as_u16(), expected_status);
        assert_eq!(head.status(), get.status());
        for name in [
            header::ACCEPT_RANGES,
            header::CONTENT_RANGE,
            header::CONTENT_LENGTH,
            header::CONTENT_DISPOSITION,
            header::CACHE_CONTROL,
            header::ETAG,
            header::CONTENT_TYPE,
            HeaderName::from_static("x-content-type-options"),
            HeaderName::from_static("cross-origin-resource-policy"),
            HeaderName::from_static("x-request-id"),
        ] {
            assert_eq!(
                head.headers().get(&name),
                get.headers().get(&name),
                "{name}"
            );
        }
        assert_eq!(
            get.headers()
                .get(header::CONTENT_RANGE)
                .map(|value| value.to_str().unwrap()),
            expected_range
        );
        assert_eq!(get.headers()[header::CONTENT_LENGTH], expected_len);
        assert!(bytes(head).await.is_empty());
    }

    let source = || MemorySource::ready("payload.bin", content.clone(), Some([0xab; 32]));
    let unsatisfiable_get = app(source())
        .oneshot(download_request(
            project,
            file,
            Method::GET,
            Some("bytes=100-"),
            None,
            true,
        ))
        .await
        .unwrap();
    let unsatisfiable_head = app(source())
        .oneshot(download_request(
            project,
            file,
            Method::HEAD,
            Some("bytes=100-"),
            None,
            true,
        ))
        .await
        .unwrap();
    assert_eq!(unsatisfiable_head.status(), unsatisfiable_get.status());
    for (name, value) in unsatisfiable_get.headers() {
        assert_eq!(
            unsatisfiable_head.headers().get(name),
            Some(value),
            "{name}"
        );
    }
    assert!(bytes(unsatisfiable_head).await.is_empty());

    let mut duplicate_if_range = download_request(
        project,
        file,
        Method::GET,
        Some("bytes=5-9"),
        Some(expected_etag),
        true,
    );
    duplicate_if_range
        .headers_mut()
        .append(header::IF_RANGE, "\"different\"".parse().unwrap());
    let response = app(source()).oneshot(duplicate_if_range).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bytes(response).await, content);
}

#[tokio::test]
async fn strong_etag_and_download_headers_are_safe_and_only_present_when_hash_is_ready() {
    let project = ProjectId::new();
    let file = FileEntryId::new();
    let dangerous = "..\\secret/evil\r\nX-Injected: yes-한글.txt";
    let response = app(MemorySource::ready(
        dangerous,
        b"hello".to_vec(),
        Some([0xab; 32]),
    ))
    .oneshot(download_request(
        project,
        file,
        Method::GET,
        None,
        None,
        true,
    ))
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/octet-stream"
    );
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(
        response.headers()["cross-origin-resource-policy"],
        "same-origin"
    );
    assert_eq!(response.headers()["x-request-id"], REQUEST_ID);
    assert_eq!(
        response.headers()[header::ETAG],
        "\"sha256-q6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6s\""
    );
    let disposition = response.headers()[header::CONTENT_DISPOSITION]
        .to_str()
        .unwrap();
    assert!(disposition.starts_with("attachment; filename=\""));
    assert!(disposition.contains("filename*=UTF-8''"));
    assert!(!disposition.contains(['\r', '\n', '/', '\\']));
    assert!(response.headers().get("x-injected").is_none());

    let no_hash = app(MemorySource::ready("plain.txt", b"hello".to_vec(), None))
        .oneshot(download_request(
            project,
            file,
            Method::GET,
            Some("bytes=1-2"),
            Some("\"sha256-q6urq6urq6urq6urq6urq6urq6urq6urq6urq6urq6s\""),
            true,
        ))
        .await
        .unwrap();
    assert_eq!(no_hash.status(), StatusCode::OK);
    assert!(no_hash.headers().get(header::ETAG).is_none());
}

#[tokio::test]
async fn large_download_is_pull_based_bounded_and_disconnect_releases_handle() {
    let project = ProjectId::new();
    let file = FileEntryId::new();
    let content = vec![0x5a; DOWNLOAD_CHUNK_BYTES * 3 + 17];
    let source = ProbeSource::new(content.clone(), ProbeBehavior::Normal);
    let response = app(source.clone())
        .oneshot(download_request(
            project,
            file,
            Method::GET,
            None,
            None,
            true,
        ))
        .await
        .unwrap();
    assert_eq!(source.reads.load(Ordering::SeqCst), 0);
    let mut body = response.into_body();
    let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert_eq!(first.len(), DOWNLOAD_CHUNK_BYTES);
    assert_eq!(source.reads.load(Ordering::SeqCst), 1);
    assert_eq!(
        source.max_request.load(Ordering::SeqCst),
        DOWNLOAD_CHUNK_BYTES
    );
    drop(body);
    assert_eq!(source.drops.load(Ordering::SeqCst), 1);

    let complete_source = ProbeSource::new(content.clone(), ProbeBehavior::Normal);
    let complete = app(complete_source.clone())
        .oneshot(download_request(
            project,
            file,
            Method::GET,
            None,
            None,
            true,
        ))
        .await
        .unwrap();
    assert_eq!(bytes(complete).await, content);
    assert_eq!(complete_source.reads.load(Ordering::SeqCst), 4);
    assert_eq!(
        complete_source.max_request.load(Ordering::SeqCst),
        DOWNLOAD_CHUNK_BYTES
    );
    assert_eq!(complete_source.drops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn short_reads_and_mid_stream_errors_are_propagated() {
    let project = ProjectId::new();
    let file = FileEntryId::new();
    for behavior in [ProbeBehavior::ShortFirst, ProbeBehavior::ErrorAfterFirst] {
        let source = ProbeSource::new(vec![0x5a; DOWNLOAD_CHUNK_BYTES * 2], behavior);
        let response = app(source.clone())
            .oneshot(download_request(
                project,
                file,
                Method::GET,
                None,
                None,
                true,
            ))
            .await
            .unwrap();
        assert!(response.into_body().collect().await.is_err());
        assert_eq!(source.drops.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn ninth_open_is_rejected_and_head_never_holds_a_streaming_permit() {
    let project = ProjectId::new();
    let file = FileEntryId::new();
    let source = ProbeSource::new(vec![0x5a; DOWNLOAD_CHUNK_BYTES], ProbeBehavior::Normal);
    let router = app(source.clone());
    let mut open = Vec::new();
    for _ in 0..8 {
        let response = router
            .clone()
            .oneshot(download_request(
                project,
                file,
                Method::GET,
                None,
                None,
                true,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        open.push(response);
    }
    assert_eq!(source.opens.load(Ordering::SeqCst), 8);
    let ninth = router
        .clone()
        .oneshot(download_request(
            project,
            file,
            Method::GET,
            None,
            None,
            true,
        ))
        .await
        .unwrap();
    assert_eq!(ninth.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(ninth.headers()[header::RETRY_AFTER], "1");
    assert_eq!(json(ninth).await["code"], "download_capacity_exhausted");
    assert_eq!(source.opens.load(Ordering::SeqCst), 8);

    let saturated_head = router
        .clone()
        .oneshot(download_request(
            project,
            file,
            Method::HEAD,
            None,
            None,
            true,
        ))
        .await
        .unwrap();
    assert_eq!(saturated_head.status(), StatusCode::OK);
    assert!(bytes(saturated_head).await.is_empty());
    assert_eq!(source.opens.load(Ordering::SeqCst), 9);

    drop(open.pop());
    let replacement = router
        .clone()
        .oneshot(download_request(
            project,
            file,
            Method::GET,
            None,
            None,
            true,
        ))
        .await
        .unwrap();
    assert_eq!(replacement.status(), StatusCode::OK);
    drop(replacement);
    drop(open);

    for _ in 0..16 {
        let head = router
            .clone()
            .oneshot(download_request(
                project,
                file,
                Method::HEAD,
                None,
                None,
                true,
            ))
            .await
            .unwrap();
        assert_eq!(head.status(), StatusCode::OK);
        assert!(bytes(head).await.is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnected_blocking_reads_retain_all_permits_and_handles_until_workers_exit() {
    let project = ProjectId::new();
    let file = FileEntryId::new();
    let source = BlockingReadSource::default();
    let router = app(source.clone());
    let mut readers = Vec::new();
    for _ in 0..MAX_OPEN_DOWNLOADS {
        let response = router
            .clone()
            .oneshot(download_request(
                project,
                file,
                Method::GET,
                None,
                None,
                true,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        readers.push(tokio::spawn(async move {
            let mut body = response.into_body();
            let _ = body.frame().await;
        }));
    }
    source
        .wait_for(&source.state.started, MAX_OPEN_DOWNLOADS)
        .await;
    for reader in &readers {
        reader.abort();
    }
    for reader in readers {
        assert!(reader.await.unwrap_err().is_cancelled());
    }
    assert!(source.every_handle_is_retained());

    let ninth = router
        .clone()
        .oneshot(download_request(
            project,
            file,
            Method::GET,
            None,
            None,
            true,
        ))
        .await
        .unwrap();
    let ninth_status = ninth.status();
    drop(ninth);

    source.release();
    source
        .wait_for(&source.state.completed, MAX_OPEN_DOWNLOADS)
        .await;
    assert!(source.every_handle_is_released());
    assert_eq!(ninth_status, StatusCode::TOO_MANY_REQUESTS);

    let replacement = router
        .oneshot(download_request(
            project,
            file,
            Method::GET,
            None,
            None,
            true,
        ))
        .await
        .unwrap();
    assert_eq!(replacement.status(), StatusCode::OK);
}

#[tokio::test]
async fn file_state_identity_auth_method_and_request_id_boundaries_fail_closed() {
    let project = ProjectId::new();
    let file = FileEntryId::new();
    for (source, status, code) in [
        (
            MemorySource::error(DownloadError::ProjectNotFound),
            StatusCode::NOT_FOUND,
            "project_not_found",
        ),
        (
            MemorySource::error(DownloadError::FileNotFound),
            StatusCode::NOT_FOUND,
            "file_not_found",
        ),
        (
            MemorySource::error(DownloadError::NotAFile),
            StatusCode::CONFLICT,
            "not_a_file",
        ),
        (
            MemorySource::error(DownloadError::Settling),
            StatusCode::CONFLICT,
            "file_settling",
        ),
        (
            MemorySource::error(DownloadError::Unsupported),
            StatusCode::CONFLICT,
            "unsupported_file_entry",
        ),
        (
            MemorySource::identity_changed("C:\\private\\secret.txt", b"secret".to_vec()),
            StatusCode::CONFLICT,
            "file_identity_changed",
        ),
        (
            MemorySource::error(DownloadError::Unavailable),
            StatusCode::SERVICE_UNAVAILABLE,
            "file_storage_unavailable",
        ),
    ] {
        let response = app(source)
            .oneshot(download_request(
                project,
                file,
                Method::GET,
                None,
                None,
                true,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        assert_eq!(response.headers()["x-request-id"], REQUEST_ID);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, no-store"
        );
        let body = json(response).await;
        assert_eq!(body["code"], code);
        assert_eq!(body["requestId"], REQUEST_ID);
        assert!(!body.to_string().contains("C:\\private"));
    }

    let unauthorized = app(MemorySource::ready("file", b"x".to_vec(), None))
        .oneshot(download_request(
            project,
            file,
            Method::GET,
            None,
            None,
            false,
        ))
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    let unauthorized = json(unauthorized).await;
    assert_eq!(unauthorized["code"], "missing_authentication");
    assert_eq!(unauthorized["requestId"], REQUEST_ID);

    let mut wrong_subject = download_request(project, file, Method::GET, None, None, false);
    let mut wrong_claims = claims();
    wrong_claims.sub = "other-owner".into();
    wrong_subject.extensions_mut().insert(wrong_claims);
    let wrong_subject = app(MemorySource::ready("file", b"x".to_vec(), None))
        .oneshot(wrong_subject)
        .await
        .unwrap();
    assert_eq!(wrong_subject.status(), StatusCode::FORBIDDEN);
    let wrong_subject = json(wrong_subject).await;
    assert_eq!(wrong_subject["code"], "claim_forbidden");
    assert_eq!(wrong_subject["requestId"], REQUEST_ID);

    let missing_csrf = app(MemorySource::ready("file", b"x".to_vec(), None))
        .oneshot(download_request(
            project,
            file,
            Method::POST,
            None,
            None,
            true,
        ))
        .await
        .unwrap();
    assert_eq!(missing_csrf.status(), StatusCode::FORBIDDEN);
    let missing_csrf = json(missing_csrf).await;
    assert_eq!(missing_csrf["code"], "csrf_forbidden");
    assert_eq!(missing_csrf["requestId"], REQUEST_ID);

    let mut duplicate_request_id = download_request(project, file, Method::GET, None, None, true);
    duplicate_request_id
        .headers_mut()
        .append("x-request-id", "duplicate".parse().unwrap());
    let duplicate_request_id = app(MemorySource::ready("file", b"x".to_vec(), None))
        .oneshot(duplicate_request_id)
        .await
        .unwrap();
    assert_eq!(duplicate_request_id.status(), StatusCode::BAD_REQUEST);
    let response_request_id = duplicate_request_id.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let duplicate_request_id = json(duplicate_request_id).await;
    assert_eq!(duplicate_request_id["code"], "invalid_request_id");
    assert_eq!(duplicate_request_id["requestId"], response_request_id);

    let mut malformed_path = Request::builder()
        .uri(format!(
            "/api/v1/projects/{project}/files/not-a-file-id/download"
        ))
        .header("x-request-id", REQUEST_ID)
        .body(Body::empty())
        .unwrap();
    malformed_path.extensions_mut().insert(claims());
    let malformed_path = app(MemorySource::ready("file", b"x".to_vec(), None))
        .oneshot(malformed_path)
        .await
        .unwrap();
    assert_eq!(malformed_path.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json(malformed_path).await["code"], "invalid_file_path");

    let protected = app(MemorySource::ready("file", b"x".to_vec(), None));
    let session = protected
        .clone()
        .oneshot({
            let mut request = Request::builder()
                .uri("/api/v1/session")
                .body(Body::empty())
                .unwrap();
            request.extensions_mut().insert(claims());
            request
        })
        .await
        .unwrap();
    let csrf = json(session).await["csrf_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut post = download_request(project, file, Method::POST, None, None, true);
    post.headers_mut()
        .insert(header::ORIGIN, ORIGIN.parse().unwrap());
    post.headers_mut().insert(
        HeaderName::from_static("x-cellar-csrf"),
        csrf.parse().unwrap(),
    );
    let response = protected.oneshot(post).await.unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(json(response).await["code"], "method_not_allowed");
}
