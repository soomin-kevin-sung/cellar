use axum::http::StatusCode;
use cellar_api::routes::session::{
    ClaimRouteError, claim_owner, claim_status, require_json_content_type,
};
use cellar_auth::{
    AccessClaims, ClaimRequest, CompareAndSet, EnrollmentError, EnrollmentMode, EnrollmentService,
    EnrollmentSnapshot, EnrollmentStore, EnrollmentStoreError, FileEnrollmentStore, RouteAccess,
};
use cellar_config::{BootstrapClaim, CellarConfig, PersistedConfig, load_config, save_config};
use std::path::PathBuf;
use std::sync::{Arc, Barrier, Mutex};
use std::thread;

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
        service.route_access("/owner/claim", &claims()).unwrap(),
        RouteAccess::OwnerClaim
    );
    assert_eq!(
        service
            .route_access("/api/v1/files", &claims())
            .unwrap_err(),
        EnrollmentError::Unavailable
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
        service.route_access("/owner/claim", &claims()).unwrap_err(),
        EnrollmentError::NotFound
    );
    assert_eq!(
        service.route_access("/api/v1/files", &claims()).unwrap(),
        RouteAccess::Authenticated
    );
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
        service.route_access("/api/v1/files", &other).unwrap_err(),
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
