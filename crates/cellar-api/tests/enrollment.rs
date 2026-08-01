use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cellar_api::routes::session::{
    ClaimRouteError, claim_owner, claim_status, require_json_content_type, session_router,
};
use cellar_auth::{
    AccessClaims, ClaimRequest, CompareAndSet, EnrollmentError, EnrollmentMode, EnrollmentService,
    EnrollmentSnapshot, EnrollmentStore, EnrollmentStoreError, FileEnrollmentStore, RouteAccess,
    route_access,
};
use cellar_config::{BootstrapClaim, CellarConfig, PersistedConfig, load_config, save_config};
use http_body_util::BodyExt;
use serde_json::Value;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::Duration;
use tower::ServiceExt;

const NOW: i64 = 10_000;
const ORIGIN: &str = "https://cellar.example";
const EMAIL: &str = "owner@example.com";
const CODE: [u8; 32] = [7; 32];

#[derive(Debug)]
struct MemoryStore {
    state: Mutex<EnrollmentSnapshot>,
    fail_saves: Mutex<usize>,
}

impl MemoryStore {
    fn unenrolled() -> Self {
        Self {
            state: Mutex::new(EnrollmentSnapshot::unenrolled(
                ORIGIN,
                EMAIL,
                BootstrapClaim::new(&CODE, NOW + 60),
            )),
            fail_saves: Mutex::new(0),
        }
    }

    fn fail_next_save(&self) {
        *self.fail_saves.lock().unwrap() += 1;
    }
}

impl EnrollmentStore for MemoryStore {
    fn load(&self) -> Result<EnrollmentSnapshot, EnrollmentStoreError> {
        Ok(self.state.lock().unwrap().clone())
    }

    fn compare_and_set_owner(
        &self,
        expected: &EnrollmentSnapshot,
        owner_subject: &str,
    ) -> Result<CompareAndSet, EnrollmentStoreError> {
        let mut state = self.state.lock().unwrap();
        if &*state != expected {
            return Ok(CompareAndSet::Changed);
        }
        let mut failures = self.fail_saves.lock().unwrap();
        if *failures > 0 {
            *failures -= 1;
            return Err(EnrollmentStoreError::new());
        }
        *state = EnrollmentSnapshot::enrolled(ORIGIN, owner_subject);
        Ok(CompareAndSet::Saved)
    }
}

fn claims() -> AccessClaims {
    AccessClaims {
        iss: "https://issuer.example".into(),
        aud: vec!["aud".into()],
        sub: "owner-subject".into(),
        email: Some(EMAIL.into()),
        exp: NOW + 300,
        nbf: NOW,
        iat: NOW,
        r#type: "app".into(),
    }
}

fn request<'a>(email: &'a str, origin: &'a str, code: &'a [u8; 32]) -> ClaimRequest<'a> {
    ClaimRequest {
        email,
        origin,
        code,
    }
}

#[test]
fn enrollment_errors_have_stable_http_mappings() {
    assert_eq!(
        claim_status(&EnrollmentError::InvalidIdentity),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        claim_status(&EnrollmentError::Forbidden),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        claim_status(&EnrollmentError::NotFound),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        claim_status(&EnrollmentError::Conflict),
        StatusCode::CONFLICT
    );
    assert_eq!(
        claim_status(&EnrollmentError::Unavailable),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        ClaimRouteError::UnsupportedMediaType.status(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    assert_eq!(
        ClaimRouteError::Enrollment(EnrollmentError::Conflict).status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        format!("{:?}", ClaimRouteError::Forbidden),
        "claim_forbidden"
    );
}

#[test]
fn claim_accepts_exactly_one_json_content_type() {
    assert!(require_json_content_type([b"application/json".as_slice()]).is_ok());
    assert!(require_json_content_type([]).is_err());
    assert!(
        require_json_content_type([
            b"application/json".as_slice(),
            b"application/json".as_slice()
        ])
        .is_err()
    );
    assert!(require_json_content_type([b"text/plain".as_slice()]).is_err());
}

#[test]
fn unenrolled_route_policy_exposes_only_claim() {
    let service = EnrollmentService::new(Arc::new(MemoryStore::unenrolled()));
    assert_eq!(
        route_access(EnrollmentMode::Unenrolled, "/owner/claim"),
        RouteAccess::ClaimOnly
    );
    assert_eq!(
        route_access(EnrollmentMode::Unenrolled, "/api/v1/files"),
        RouteAccess::Deny
    );
    service
        .authorize(RouteAccess::ClaimOnly, &claims())
        .unwrap();
}

#[test]
fn pure_route_policy_has_exact_enrollment_mapping() {
    assert_eq!(
        route_access(EnrollmentMode::Enrolled, "/owner/claim"),
        RouteAccess::NotFound
    );
    assert_eq!(
        route_access(EnrollmentMode::Enrolled, "/api/v1/files"),
        RouteAccess::OwnerOnly
    );
}

#[test]
fn successful_claim_enrolls_exact_subject_and_disables_claim() {
    let service = EnrollmentService::new(Arc::new(MemoryStore::unenrolled()));
    assert_eq!(
        service
            .claim(&claims(), request(EMAIL, ORIGIN, &CODE), NOW)
            .unwrap(),
        EnrollmentMode::Enrolled
    );
    assert_eq!(
        service
            .authorize(
                route_access(service.mode().unwrap(), "/owner/claim"),
                &claims(),
            )
            .unwrap_err(),
        EnrollmentError::NotFound
    );
    service
        .authorize(RouteAccess::OwnerOnly, &claims())
        .unwrap();
}

#[test]
fn claim_fails_closed_for_exact_email_origin_code_expiry_and_subject() {
    for (mut claim, input, now) in [
        (claims(), request("Owner@example.com", ORIGIN, &CODE), NOW),
        (
            claims(),
            request(EMAIL, "https://cellar.example/", &CODE),
            NOW,
        ),
        (claims(), request(EMAIL, ORIGIN, &[9; 32]), NOW),
        (claims(), request(EMAIL, ORIGIN, &CODE), NOW + 60),
    ] {
        let service = EnrollmentService::new(Arc::new(MemoryStore::unenrolled()));
        assert_eq!(
            service.claim(&claim, input, now).unwrap_err(),
            EnrollmentError::Forbidden
        );
        claim.sub.clear();
        assert_eq!(
            service
                .claim(&claim, request(EMAIL, ORIGIN, &CODE), NOW)
                .unwrap_err(),
            EnrollmentError::InvalidIdentity
        );
    }
}

#[test]
fn failed_durable_save_leaves_claim_retryable() {
    let store = Arc::new(MemoryStore::unenrolled());
    store.fail_next_save();
    let service = EnrollmentService::new(store);
    assert_eq!(
        service
            .claim(&claims(), request(EMAIL, ORIGIN, &CODE), NOW)
            .unwrap_err(),
        EnrollmentError::Unavailable
    );
    assert_eq!(
        service
            .claim(&claims(), request(EMAIL, ORIGIN, &CODE), NOW)
            .unwrap(),
        EnrollmentMode::Enrolled
    );
}

#[test]
fn concurrent_valid_claims_have_exactly_one_winner() {
    let service = Arc::new(EnrollmentService::new(Arc::new(MemoryStore::unenrolled())));
    let barrier = Arc::new(Barrier::new(9));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let service = Arc::clone(&service);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                service.claim(&claims(), request(EMAIL, ORIGIN, &CODE), NOW)
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert!(
        results
            .iter()
            .filter(|result| result.is_err())
            .all(|result| {
                matches!(
                    result,
                    Err(EnrollmentError::Conflict | EnrollmentError::NotFound)
                )
            })
    );
}

#[test]
fn enrolled_routes_require_the_immutable_subject() {
    let store = Arc::new(MemoryStore::unenrolled());
    let service = EnrollmentService::new(store);
    service
        .claim(&claims(), request(EMAIL, ORIGIN, &CODE), NOW)
        .unwrap();
    let mut other = claims();
    other.sub = "someone-else".into();
    assert_eq!(
        service
            .authorize(RouteAccess::OwnerOnly, &other)
            .unwrap_err(),
        EnrollmentError::Forbidden
    );
}

#[test]
fn file_adapter_durably_clears_bootstrap_material_and_keeps_owner_immutable() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    save_config(
        &path,
        &PersistedConfig {
            config: CellarConfig {
                external_origin: "https://cellar.example".parse().unwrap(),
                team_domain: "https://team.cloudflareaccess.com".parse().unwrap(),
                aud_tags: vec!["aud".into()],
                bootstrap_owner_email: Some(EMAIL.into()),
                owner_subject: None,
                storage_root: PathBuf::from(r"C:\cellar-storage"),
                origin_port: 8443,
                health_port: 8081,
            },
            bootstrap_claim: Some(BootstrapClaim::new(&CODE, NOW + 60)),
        },
    )
    .unwrap();
    let store = Arc::new(FileEnrollmentStore::new(&path));
    let service = EnrollmentService::new(Arc::clone(&store));
    service
        .claim(&claims(), request(EMAIL, ORIGIN, &CODE), NOW)
        .unwrap();

    let persisted = load_config(&path).unwrap();
    assert_eq!(persisted.config.bootstrap_owner_email, None);
    assert_eq!(persisted.bootstrap_claim, None);
    assert_eq!(
        persisted.config.owner_subject.as_deref(),
        Some("owner-subject")
    );
    let enrolled = store.load().unwrap();
    assert_eq!(
        store
            .compare_and_set_owner(&enrolled, "replacement")
            .unwrap(),
        CompareAndSet::Changed
    );
    assert_eq!(
        load_config(&path).unwrap().config.owner_subject.as_deref(),
        Some("owner-subject")
    );
}

#[test]
fn claim_route_requires_one_declared_json_type_and_one_origin() {
    let service = EnrollmentService::new(Arc::new(MemoryStore::unenrolled()));
    let json = b"application/json".as_slice();
    let origin = ORIGIN.as_bytes();
    assert_eq!(
        claim_owner(
            &service,
            &claims(),
            &[json],
            &[origin, origin],
            EMAIL,
            &CODE,
            NOW,
        )
        .unwrap_err(),
        ClaimRouteError::Forbidden
    );
    assert_eq!(
        claim_owner(&service, &claims(), &[], &[origin], EMAIL, &CODE, NOW).unwrap_err(),
        ClaimRouteError::UnsupportedMediaType
    );
}

#[test]
fn independent_file_adapters_still_allow_exactly_one_claim_winner() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    save_config(
        &path,
        &PersistedConfig {
            config: CellarConfig {
                external_origin: "https://cellar.example".parse().unwrap(),
                team_domain: "https://team.cloudflareaccess.com".parse().unwrap(),
                aud_tags: (0..2_000).map(|index| format!("aud-{index}")).collect(),
                bootstrap_owner_email: Some(EMAIL.into()),
                owner_subject: None,
                storage_root: PathBuf::from(r"C:\cellar-storage"),
                origin_port: 8443,
                health_port: 8081,
            },
            bootstrap_claim: Some(BootstrapClaim::new(&CODE, NOW + 60)),
        },
    )
    .unwrap();
    let first = EnrollmentService::new(Arc::new(FileEnrollmentStore::new(&path)));
    let second = EnrollmentService::new(Arc::new(FileEnrollmentStore::new(&path)));
    let barrier = Arc::new(Barrier::new(3));
    let handles: Vec<_> = [first, second]
        .into_iter()
        .map(|service| {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                service.claim(&claims(), request(EMAIL, ORIGIN, &CODE), NOW)
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
}

fn http_request(method: &str, uri: &str, body: Body, authenticated: bool) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .body(body)
        .unwrap();
    if authenticated {
        request.extensions_mut().insert(claims());
    }
    request
}

#[tokio::test]
async fn http_claim_requires_auth_and_is_the_csrf_exemption_then_disappears() {
    let service = EnrollmentService::new(Arc::new(MemoryStore::unenrolled()));
    let app = session_router(service, Arc::new(cellar_auth::CsrfManager::new()), || NOW);
    let code = URL_SAFE_NO_PAD.encode(CODE);
    let body = || Body::from(format!(r#"{{"email":"{EMAIL}","claim_code":"{code}"}}"#));

    let mut missing_auth = http_request("POST", "/owner/claim", body(), false);
    missing_auth
        .headers_mut()
        .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    missing_auth
        .headers_mut()
        .insert(header::ORIGIN, ORIGIN.parse().unwrap());
    let response = app.clone().oneshot(missing_auth).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(
        response
            .headers()
            .get("access-control-allow-origin")
            .is_none()
    );
    let error: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        error,
        serde_json::json!({"error": "missing_authentication"})
    );

    let mut claim = http_request("POST", "/owner/claim", body(), true);
    claim
        .headers_mut()
        .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    claim
        .headers_mut()
        .insert(header::ORIGIN, ORIGIN.parse().unwrap());
    assert_eq!(
        app.clone().oneshot(claim).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );

    let mut reused = http_request("POST", "/owner/claim", body(), true);
    reused
        .headers_mut()
        .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    reused
        .headers_mut()
        .insert(header::ORIGIN, ORIGIN.parse().unwrap());
    let response = app.oneshot(reused).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let error: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(error, serde_json::json!({"error": "claim_not_found"}));
}

#[tokio::test]
async fn http_claim_rejects_duplicate_origin_and_unenrolled_non_claim_routes() {
    let service = EnrollmentService::new(Arc::new(MemoryStore::unenrolled()));
    let app = session_router(service, Arc::new(cellar_auth::CsrfManager::new()), || NOW);
    let code = URL_SAFE_NO_PAD.encode(CODE);
    let mut claim = http_request(
        "POST",
        "/owner/claim",
        Body::from(format!(r#"{{"email":"{EMAIL}","claim_code":"{code}"}}"#)),
        true,
    );
    claim
        .headers_mut()
        .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    claim
        .headers_mut()
        .append(header::ORIGIN, ORIGIN.parse().unwrap());
    claim
        .headers_mut()
        .append(header::ORIGIN, ORIGIN.parse().unwrap());
    assert_eq!(
        app.clone().oneshot(claim).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );

    let request = http_request("GET", "/api/v1/files", Body::empty(), true);
    assert_eq!(
        app.oneshot(request).await.unwrap().status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[test]
fn independent_processes_have_exactly_one_durable_claim_winner() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let start = directory.path().join("start");
    save_config(
        &path,
        &PersistedConfig {
            config: CellarConfig {
                external_origin: "https://cellar.example".parse().unwrap(),
                team_domain: "https://team.cloudflareaccess.com".parse().unwrap(),
                aud_tags: (0..2_000).map(|index| format!("aud-{index}")).collect(),
                bootstrap_owner_email: Some(EMAIL.into()),
                owner_subject: None,
                storage_root: PathBuf::from(r"C:\cellar-storage"),
                origin_port: 8443,
                health_port: 8081,
            },
            bootstrap_claim: Some(BootstrapClaim::new(&CODE, NOW + 60)),
        },
    )
    .unwrap();

    let child = |subject: &str| {
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "enrollment_child_process_helper",
                "--nocapture",
            ])
            .env("CELLAR_TEST_CONFIG", &path)
            .env("CELLAR_TEST_START", &start)
            .env("CELLAR_TEST_SUBJECT", subject)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    };
    let first = child("process-owner-one");
    let second = child("process-owner-two");
    std::fs::write(&start, b"go").unwrap();
    let statuses = [
        first.wait_with_output().unwrap().status,
        second.wait_with_output().unwrap().status,
    ];
    assert_eq!(statuses.iter().filter(|status| status.success()).count(), 1);

    let winner = load_config(&path).unwrap().config.owner_subject.unwrap();
    assert!(matches!(
        winner.as_str(),
        "process-owner-one" | "process-owner-two"
    ));
}

#[test]
#[ignore = "spawned by independent_processes_have_exactly_one_durable_claim_winner"]
fn enrollment_child_process_helper() {
    let Ok(path) = std::env::var("CELLAR_TEST_CONFIG") else {
        return;
    };
    let start = PathBuf::from(std::env::var("CELLAR_TEST_START").unwrap());
    while !start.exists() {
        thread::sleep(Duration::from_millis(1));
    }
    let mut child_claims = claims();
    child_claims.sub = std::env::var("CELLAR_TEST_SUBJECT").unwrap();
    let service = EnrollmentService::new(Arc::new(FileEnrollmentStore::new(path)));
    match service.claim(&child_claims, request(EMAIL, ORIGIN, &CODE), NOW) {
        Ok(EnrollmentMode::Enrolled) => {}
        Err(EnrollmentError::NotFound | EnrollmentError::Conflict) => std::process::exit(10),
        Err(error) => panic!("unexpected child enrollment error: {error}"),
        Ok(EnrollmentMode::Unenrolled) => panic!("claim did not enroll"),
    }
}

#[test]
fn sidecar_lock_acquisition_failure_does_not_consume_claim() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    save_config(
        &path,
        &PersistedConfig {
            config: CellarConfig {
                external_origin: "https://cellar.example".parse().unwrap(),
                team_domain: "https://team.cloudflareaccess.com".parse().unwrap(),
                aud_tags: vec!["aud".into()],
                bootstrap_owner_email: Some(EMAIL.into()),
                owner_subject: None,
                storage_root: PathBuf::from(r"C:\cellar-storage"),
                origin_port: 8443,
                health_port: 8081,
            },
            bootstrap_claim: Some(BootstrapClaim::new(&CODE, NOW + 60)),
        },
    )
    .unwrap();
    let lock_path = directory.path().join("config.toml.enrollment.lock");
    std::fs::create_dir(&lock_path).unwrap();
    let service = EnrollmentService::new(Arc::new(FileEnrollmentStore::new(&path)));
    assert_eq!(
        service
            .claim(&claims(), request(EMAIL, ORIGIN, &CODE), NOW)
            .unwrap_err(),
        EnrollmentError::Unavailable
    );
    std::fs::remove_dir(lock_path).unwrap();
    assert_eq!(
        service
            .claim(&claims(), request(EMAIL, ORIGIN, &CODE), NOW)
            .unwrap(),
        EnrollmentMode::Enrolled
    );
}

#[test]
fn file_store_debug_redacts_the_config_path() {
    let store = FileEnrollmentStore::new(r"C:\secret-owner-folder\config.toml");
    let debug = format!("{store:?}");
    assert!(!debug.contains("secret-owner-folder"));
    assert!(debug.contains("<redacted>"));
}
