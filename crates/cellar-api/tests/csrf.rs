use axum::Router;
use axum::body::Body;
use axum::http::{HeaderName, Request, StatusCode, header};
use axum::routing::any;
use cellar_api::routes::session::{
    SessionRouteError, csrf_status, get_session, session_router_with_routes,
};
use cellar_auth::{
    AccessClaims, CompareAndSet, CsrfError, CsrfManager, EnrollmentService, EnrollmentSnapshot,
    EnrollmentStore, EnrollmentStoreError, MutationHeaders,
};
use http_body_util::BodyExt;
use serde_json::Value;
use std::sync::{Arc, Barrier};
use std::thread;
use tower::ServiceExt;

const NOW: i64 = 50_000;
const ORIGIN: &str = "https://cellar.example";
const SUBJECT: &str = "owner-subject";

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

fn headers<'a>(origin: &'a [u8], token: &'a [u8]) -> MutationHeaders<'a> {
    MutationHeaders {
        origins: vec![origin],
        csrf_tokens: vec![token],
        sec_fetch_site: vec![],
    }
}

#[test]
fn csrf_errors_have_stable_http_mappings() {
    assert_eq!(
        csrf_status(&CsrfError::Unauthenticated),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(csrf_status(&CsrfError::Forbidden), StatusCode::FORBIDDEN);
    assert_eq!(
        csrf_status(&CsrfError::Unavailable),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        SessionRouteError::Csrf(CsrfError::Unauthenticated).status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(SessionRouteError::NotFound.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        format!("{:?}", SessionRouteError::Csrf(CsrfError::Forbidden)),
        "csrf_forbidden"
    );
}

#[test]
fn session_issue_returns_base64url_token_and_rotation_invalidates_prior_token() {
    let manager = CsrfManager::new();
    let first = manager.issue(&claims(), SUBJECT, NOW).unwrap();
    assert_eq!(first.len(), 43);
    assert!(!first.contains('='));
    let second = manager.issue(&claims(), SUBJECT, NOW + 1).unwrap();
    assert_ne!(first, second);
    assert_eq!(
        manager
            .validate_mutation(
                "POST",
                headers(ORIGIN.as_bytes(), first.as_bytes()),
                &claims(),
                SUBJECT,
                ORIGIN,
                NOW + 1,
            )
            .unwrap_err(),
        CsrfError::Forbidden
    );
    manager
        .validate_mutation(
            "POST",
            headers(ORIGIN.as_bytes(), second.as_bytes()),
            &claims(),
            SUBJECT,
            ORIGIN,
            NOW + 1,
        )
        .unwrap();
}

#[test]
fn restart_and_access_window_change_invalidate_tokens() {
    let manager = CsrfManager::new();
    let token = manager.issue(&claims(), SUBJECT, NOW).unwrap();
    assert_eq!(
        CsrfManager::new()
            .validate_mutation(
                "DELETE",
                headers(ORIGIN.as_bytes(), token.as_bytes()),
                &claims(),
                SUBJECT,
                ORIGIN,
                NOW + 1,
            )
            .unwrap_err(),
        CsrfError::Forbidden
    );
    let mut changed = claims();
    changed.iat += 1;
    assert_eq!(
        manager
            .validate_mutation(
                "PUT",
                headers(ORIGIN.as_bytes(), token.as_bytes()),
                &changed,
                SUBJECT,
                ORIGIN,
                NOW + 1,
            )
            .unwrap_err(),
        CsrfError::Forbidden
    );
}

#[test]
fn mutation_guard_rejects_duplicate_missing_malformed_and_cross_site_headers() {
    let manager = CsrfManager::new();
    let token = manager.issue(&claims(), SUBJECT, NOW).unwrap();
    let origin = ORIGIN.as_bytes();
    let token = token.as_bytes();
    let cases = [
        MutationHeaders {
            origins: vec![],
            csrf_tokens: vec![token],
            sec_fetch_site: vec![],
        },
        MutationHeaders {
            origins: vec![origin, origin],
            csrf_tokens: vec![token],
            sec_fetch_site: vec![],
        },
        MutationHeaders {
            origins: vec![b"null"],
            csrf_tokens: vec![token],
            sec_fetch_site: vec![],
        },
        MutationHeaders {
            origins: vec![origin],
            csrf_tokens: vec![],
            sec_fetch_site: vec![],
        },
        MutationHeaders {
            origins: vec![origin],
            csrf_tokens: vec![token, token],
            sec_fetch_site: vec![],
        },
        MutationHeaders {
            origins: vec![origin],
            csrf_tokens: vec![b"not base64!"],
            sec_fetch_site: vec![],
        },
        MutationHeaders {
            origins: vec![origin],
            csrf_tokens: vec![token],
            sec_fetch_site: vec![b"cross-site"],
        },
    ];
    for candidate in cases {
        assert_eq!(
            manager
                .validate_mutation("PATCH", candidate, &claims(), SUBJECT, ORIGIN, NOW + 1)
                .unwrap_err(),
            CsrfError::Forbidden
        );
    }
}

#[test]
fn mutation_guard_rejects_wrong_subject_token_and_exact_expiries() {
    let manager = CsrfManager::new();
    let token = manager.issue(&claims(), SUBJECT, NOW).unwrap();
    let token_headers = || headers(ORIGIN.as_bytes(), token.as_bytes());
    let mut wrong = claims();
    wrong.sub = "other".into();
    assert_eq!(
        manager
            .validate_mutation("POST", token_headers(), &wrong, SUBJECT, ORIGIN, NOW + 1)
            .unwrap_err(),
        CsrfError::Unauthenticated
    );
    assert_eq!(
        manager
            .validate_mutation(
                "POST",
                headers(
                    ORIGIN.as_bytes(),
                    b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
                ),
                &claims(),
                SUBJECT,
                ORIGIN,
                NOW + 1
            )
            .unwrap_err(),
        CsrfError::Forbidden
    );
    assert_eq!(
        manager
            .validate_mutation(
                "POST",
                token_headers(),
                &claims(),
                SUBJECT,
                ORIGIN,
                NOW + 8 * 60 * 60
            )
            .unwrap_err(),
        CsrfError::Forbidden
    );
    let mut expiring = claims();
    expiring.exp = NOW + 2;
    let expiring_token = manager.issue(&expiring, SUBJECT, NOW + 1).unwrap();
    assert_eq!(
        manager
            .validate_mutation(
                "POST",
                headers(ORIGIN.as_bytes(), expiring_token.as_bytes()),
                &expiring,
                SUBJECT,
                ORIGIN,
                NOW + 2
            )
            .unwrap_err(),
        CsrfError::Unauthenticated
    );
}

#[test]
fn safe_methods_do_not_require_csrf() {
    let manager = CsrfManager::new();
    for method in ["GET", "HEAD", "OPTIONS", "TRACE"] {
        manager
            .validate_mutation(
                method,
                MutationHeaders::empty(),
                &claims(),
                SUBJECT,
                ORIGIN,
                NOW,
            )
            .unwrap();
    }
}

#[test]
fn concurrent_issue_leaves_exactly_one_current_token_without_mismatch() {
    let manager = Arc::new(CsrfManager::new());
    let barrier = Arc::new(Barrier::new(9));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let manager = Arc::clone(&manager);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                manager.issue(&claims(), SUBJECT, NOW).unwrap()
            })
        })
        .collect();
    barrier.wait();
    let tokens: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    let valid = tokens
        .iter()
        .filter(|token| {
            manager
                .validate_mutation(
                    "POST",
                    headers(ORIGIN.as_bytes(), token.as_bytes()),
                    &claims(),
                    SUBJECT,
                    ORIGIN,
                    NOW,
                )
                .is_ok()
        })
        .count();
    assert_eq!(valid, 1);
}

#[test]
fn session_route_issues_only_for_authenticated_get() {
    let manager = CsrfManager::new();
    let response = get_session("GET", &manager, &claims(), SUBJECT, NOW).unwrap();
    assert_eq!(response.csrf_token.len(), 43);
    assert_eq!(
        get_session("POST", &manager, &claims(), SUBJECT, NOW).unwrap_err(),
        SessionRouteError::NotFound
    );
    let mut other = claims();
    other.sub = "other".into();
    assert_eq!(
        get_session("GET", &manager, &other, SUBJECT, NOW).unwrap_err(),
        SessionRouteError::Csrf(CsrfError::Unauthenticated)
    );
}

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

fn http_request(
    method: &str,
    uri: &str,
    authenticated_claims: Option<AccessClaims>,
) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    if let Some(claims) = authenticated_claims {
        request.extensions_mut().insert(claims);
    }
    request
}

async fn issued_token(app: &axum::Router, claims: AccessClaims) -> String {
    let response = app
        .clone()
        .oneshot(http_request("GET", "/api/v1/session", Some(claims)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice::<Value>(&body).unwrap()["csrf_token"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn enrolled_app() -> axum::Router {
    let protected = Router::new().route("/api/v1/files", any(|| async { StatusCode::NO_CONTENT }));
    session_router_with_routes(
        EnrollmentService::new(Arc::new(EnrolledStore)),
        Arc::new(CsrfManager::new()),
        || NOW + 10,
        protected,
    )
}

fn add_mutation_headers(request: &mut Request<Body>, token: &str) {
    request
        .headers_mut()
        .insert(header::ORIGIN, ORIGIN.parse().unwrap());
    request.headers_mut().insert(
        HeaderName::from_static("x-cellar-csrf"),
        token.parse().unwrap(),
    );
}

#[tokio::test]
async fn http_session_and_mutations_fail_closed_without_auth_or_csrf() {
    let app = enrolled_app();
    for (method, uri) in [("GET", "/api/v1/session"), ("POST", "/api/v1/files")] {
        let response = app
            .clone()
            .oneshot(http_request(method, uri, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(
            response
                .headers()
                .get("access-control-allow-origin")
                .is_none()
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            serde_json::json!({"error": "missing_authentication"})
        );
    }
    let response = app
        .oneshot(http_request("POST", "/api/v1/files", Some(claims())))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn http_mutation_rejects_duplicate_wrong_and_cross_site_headers() {
    let app = enrolled_app();
    let token = issued_token(&app, claims()).await;

    for duplicate_origin in [true, false] {
        let mut duplicate = http_request("POST", "/api/v1/files", Some(claims()));
        duplicate
            .headers_mut()
            .append(header::ORIGIN, ORIGIN.parse().unwrap());
        if duplicate_origin {
            duplicate
                .headers_mut()
                .append(header::ORIGIN, ORIGIN.parse().unwrap());
        }
        duplicate.headers_mut().append(
            HeaderName::from_static("x-cellar-csrf"),
            token.parse().unwrap(),
        );
        if !duplicate_origin {
            duplicate.headers_mut().append(
                HeaderName::from_static("x-cellar-csrf"),
                token.parse().unwrap(),
            );
        }
        assert_eq!(
            app.clone().oneshot(duplicate).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }

    for (origin, candidate, fetch_site) in [
        (ORIGIN, "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", None),
        ("https://evil.example", token.as_str(), Some("cross-site")),
    ] {
        let mut request = http_request("DELETE", "/api/v1/files", Some(claims()));
        request
            .headers_mut()
            .insert(header::ORIGIN, origin.parse().unwrap());
        request.headers_mut().insert(
            HeaderName::from_static("x-cellar-csrf"),
            candidate.parse().unwrap(),
        );
        if let Some(value) = fetch_site {
            request.headers_mut().insert(
                HeaderName::from_static("sec-fetch-site"),
                value.parse().unwrap(),
            );
        }
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }
}

#[tokio::test]
async fn http_csrf_binding_rejects_both_iat_and_exp_changes() {
    let app = enrolled_app();
    let token = issued_token(&app, claims()).await;
    let mut changed_iat = claims();
    changed_iat.iat += 1;
    let mut changed_exp = claims();
    changed_exp.exp += 1;
    for changed in [changed_iat, changed_exp] {
        let mut request = http_request("PATCH", "/api/v1/files", Some(changed));
        add_mutation_headers(&mut request, &token);
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }
}

#[tokio::test]
async fn http_safe_methods_require_owner_but_not_csrf_and_emit_no_cors() {
    let app = enrolled_app();
    for method in ["GET", "HEAD", "OPTIONS", "TRACE"] {
        let response = app
            .clone()
            .oneshot(http_request(method, "/api/v1/files", Some(claims())))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(
            response
                .headers()
                .get("access-control-allow-origin")
                .is_none()
        );
    }
    let mut other = claims();
    other.sub = "other".into();
    assert_eq!(
        app.oneshot(http_request("GET", "/api/v1/files", Some(other)))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn http_valid_csrf_reaches_protected_mutation_handler() {
    let app = enrolled_app();
    let token = issued_token(&app, claims()).await;
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let mut request = http_request(method, "/api/v1/files", Some(claims()));
        add_mutation_headers(&mut request, &token);
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::NO_CONTENT
        );
    }
}

#[tokio::test]
async fn http_custom_unsafe_method_cannot_bypass_csrf() {
    let app = enrolled_app();
    let request = http_request("MKCOL", "/api/v1/files", Some(claims()));
    assert_eq!(
        app.oneshot(request).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
}

#[test]
fn malformed_or_unknown_fetch_site_values_fail_closed() {
    let manager = CsrfManager::new();
    let token = manager.issue(&claims(), SUBJECT, NOW).unwrap();
    for value in [b"".as_slice(), b"unknown", b"cross-site, same-origin"] {
        assert_eq!(
            manager
                .validate_mutation(
                    "POST",
                    MutationHeaders {
                        origins: vec![ORIGIN.as_bytes()],
                        csrf_tokens: vec![token.as_bytes()],
                        sec_fetch_site: vec![value],
                    },
                    &claims(),
                    SUBJECT,
                    ORIGIN,
                    NOW,
                )
                .unwrap_err(),
            CsrfError::Forbidden
        );
    }
}
