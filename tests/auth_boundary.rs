use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{
    Extension, Router,
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header},
    routing::{get, post},
};
use cellar::{
    app::{RequestId, X_REQUEST_ID, secure_api_router},
    auth::{AccessError, AccessFailure, AccessVerifier, OwnerIdentity},
    config::Config,
    error::AppError,
};
use serde_json::Value;
use tower::ServiceExt;

const ASSERTION_HEADER: &str = "Cf-Access-Jwt-Assertion";
const OWNER: &str = "owner@example.com";
const ORIGIN: &str = "https://files.example.com";

#[derive(Clone)]
struct FakeVerifier {
    calls: Arc<AtomicUsize>,
}

impl FakeVerifier {
    fn new() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl AccessVerifier for FakeVerifier {
    fn verify<'a>(
        &'a self,
        assertion: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<OwnerIdentity, AccessError>> + Send + 'a>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            match assertion {
                "valid-token" => OwnerIdentity::try_from_email(OWNER),
                "forbidden-token" => Err(AccessError::forbidden()),
                "unavailable-token" => Err(AccessError::unavailable()),
                _ => Err(AccessError::unauthenticated()),
            }
        })
    }
}

fn config() -> Config {
    Config::parse(
        r#"bind = "127.0.0.1:8787"
external_origin = "https://files.example.com"
data_root = "D:/CellarData"
database_path = "D:/CellarData/.cellar/cellar.db"

[access]
team_domain = "https://example.cloudflareaccess.com"
audience = "test-audience"
owner_email = "owner@example.com"
"#,
    )
    .unwrap()
}

fn protected_router(verifier: FakeVerifier) -> Router {
    async fn identity(Extension(identity): Extension<OwnerIdentity>) -> String {
        identity.email().to_owned()
    }

    let config = config();
    secure_api_router(
        Router::new().route("/identity", get(identity)).route(
            "/write",
            post(identity)
                .put(identity)
                .patch(identity)
                .delete(identity),
        ),
        Arc::new(verifier),
        config.external_origin(),
    )
}

fn request(method: Method, path: &str) -> http::request::Builder {
    Request::builder()
        .method(method)
        .uri(path)
        .header(ASSERTION_HEADER, "valid-token")
}

async fn json_body(response: axum::response::Response) -> Value {
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn valid_assertion_inserts_normalized_owner_identity() {
    let app = protected_router(FakeVerifier::new());
    let response = app
        .oneshot(
            request(Method::GET, "/identity")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().contains_key(&X_REQUEST_ID));
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        OWNER
    );
}

#[tokio::test]
async fn missing_assertion_is_unauthorized_without_calling_verifier() {
    let verifier = FakeVerifier::new();
    let calls = verifier.calls.clone();
    let app = protected_router(verifier);
    let response = app
        .oneshot(Request::get("/identity").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(json_body(response).await["error"]["code"], "unauthorized");
}

#[tokio::test]
async fn cookie_and_spoofed_identity_header_are_not_accepted_as_proof() {
    let app = protected_router(FakeVerifier::new());
    let response = app
        .oneshot(
            Request::get("/identity")
                .header(header::COOKIE, "CF_Authorization=not-proof")
                .header("Cf-Access-Authenticated-User-Email", "owner@example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn multiple_assertion_headers_are_unauthorized() {
    let mut request = Request::get("/identity").body(Body::empty()).unwrap();
    request
        .headers_mut()
        .append(ASSERTION_HEADER, "valid-token".parse().unwrap());
    request
        .headers_mut()
        .append(ASSERTION_HEADER, "valid-token".parse().unwrap());

    let response = protected_router(FakeVerifier::new())
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn malformed_or_invalid_assertion_is_unauthorized() {
    let app = protected_router(FakeVerifier::new());
    let response = app
        .oneshot(
            Request::get("/identity")
                .header(ASSERTION_HEADER, "invalid-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(json_body(response).await["error"]["code"], "unauthorized");
}

#[tokio::test]
async fn malformed_assertion_header_bytes_are_unauthorized() {
    let mut request = Request::get("/identity").body(Body::empty()).unwrap();
    request.headers_mut().insert(
        ASSERTION_HEADER,
        header::HeaderValue::from_bytes(&[0xff]).unwrap(),
    );
    let response = protected_router(FakeVerifier::new())
        .oneshot(request)
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn verifier_classification_maps_to_safe_status_and_matching_request_id() {
    for (token, status, code, message) in [
        (
            "invalid-token",
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Authentication is required.",
        ),
        (
            "forbidden-token",
            StatusCode::FORBIDDEN,
            "forbidden",
            "You do not have permission to perform this action.",
        ),
        (
            "unavailable-token",
            StatusCode::SERVICE_UNAVAILABLE,
            "authentication_unavailable",
            "Authentication is temporarily unavailable.",
        ),
    ] {
        let response = protected_router(FakeVerifier::new())
            .oneshot(
                Request::get("/identity")
                    .header(ASSERTION_HEADER, token)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), status, "token {token}");
        let request_id = response.headers().get(&X_REQUEST_ID).unwrap().clone();
        let body = json_body(response).await;
        assert_eq!(body["error"]["code"], code);
        assert_eq!(body["error"]["message"], message);
        assert_eq!(body["error"]["requestId"], request_id.to_str().unwrap());
    }
}

#[tokio::test]
async fn spoofed_email_header_cannot_replace_assertion_identity() {
    let app = protected_router(FakeVerifier::new());
    let response = app
        .oneshot(
            request(Method::GET, "/identity")
                .header("Cf-Access-Authenticated-User-Email", "attacker@example.com")
                .header(header::COOKIE, "CF_Authorization=fake")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        OWNER
    );
}

#[tokio::test]
async fn unsafe_methods_require_one_exact_origin() {
    for (label, origins) in [
        ("missing", vec![]),
        ("null", vec!["null"]),
        ("wrong scheme", vec!["http://files.example.com"]),
        ("wrong host", vec!["https://evil.example.com"]),
        ("wrong port", vec!["https://files.example.com:8443"]),
        (
            "suffix lookalike",
            vec!["https://files.example.com.evil.test"],
        ),
        ("prefix lookalike", vec!["https://evil-files.example.com"]),
        ("trailing slash", vec!["https://files.example.com/"]),
        (
            "comma-separated",
            vec!["https://files.example.com, https://evil.example.com"],
        ),
        (
            "multiple",
            vec!["https://files.example.com", "https://files.example.com"],
        ),
    ] {
        let app = protected_router(FakeVerifier::new());
        let mut builder = request(Method::POST, "/write");
        for origin in origins {
            builder = builder.header(header::ORIGIN, origin);
        }
        let response = app
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "case {label}");
    }

    for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
        let app = protected_router(FakeVerifier::new());
        let response = app
            .oneshot(
                request(method.clone(), "/write")
                    .header(header::ORIGIN, ORIGIN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "method {method}");
    }

    let app = protected_router(FakeVerifier::new());
    let mut invalid_bytes = Request::post("/write")
        .header(ASSERTION_HEADER, "valid-token")
        .body(Body::empty())
        .unwrap();
    invalid_bytes.headers_mut().insert(
        header::ORIGIN,
        header::HeaderValue::from_bytes(&[0xff]).unwrap(),
    );
    let response = app.oneshot(invalid_bytes).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn safe_methods_pass_without_origin_and_options_adds_no_cors() {
    for method in [Method::GET, Method::HEAD, Method::OPTIONS] {
        let app = protected_router(FakeVerifier::new());
        let response = app
            .oneshot(
                request(method.clone(), "/identity")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(response.status(), StatusCode::FORBIDDEN, "method {method}");
        assert!(
            response
                .headers()
                .keys()
                .all(|name| !name.as_str().starts_with("access-control-allow-"))
        );
    }
}

#[tokio::test]
async fn request_id_header_matches_authentication_error_body() {
    let app = protected_router(FakeVerifier::new());
    let response = app
        .oneshot(Request::get("/identity").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let request_id = response.headers().get(&X_REQUEST_ID).unwrap().clone();
    let body = json_body(response).await;

    assert_eq!(body["error"]["requestId"], request_id.to_str().unwrap());
}

#[tokio::test]
async fn request_id_header_matches_authenticated_handler_error_body() {
    async fn failure(
        Extension(request_id): Extension<RequestId>,
        Extension(_identity): Extension<OwnerIdentity>,
    ) -> AppError {
        AppError::bad_request(request_id, "test_error", "Test error.")
    }

    let config = config();
    let app = secure_api_router(
        Router::new().route("/failure", get(failure)),
        Arc::new(FakeVerifier::new()),
        config.external_origin(),
    );
    let response = app
        .oneshot(
            request(Method::GET, "/failure")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let request_id = response.headers().get(&X_REQUEST_ID).unwrap().clone();
    let body = json_body(response).await;

    assert_eq!(body["error"]["requestId"], request_id.to_str().unwrap());
}

#[tokio::test]
async fn unsecured_static_router_stays_outside_only_when_not_passed_to_security_function() {
    let config = config();
    let api = secure_api_router(
        Router::new().route("/identity", get(|| async { "api" })),
        Arc::new(FakeVerifier::new()),
        config.external_origin(),
    );
    let app = Router::new()
        .route("/", get(|| async { "static" }))
        .nest("/api", api);

    let static_response = app
        .clone()
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let api_response = app
        .oneshot(Request::get("/api/identity").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(static_response.status(), StatusCode::OK);
    assert_eq!(api_response.status(), StatusCode::UNAUTHORIZED);
}

#[test]
fn identity_and_access_errors_redact_sensitive_values() {
    let identity = OwnerIdentity::try_from_email(" Secret.Owner@Example.COM ").unwrap();
    let error = AccessError::unauthenticated();

    assert_eq!(identity.email(), "secret.owner@example.com");
    assert!(!format!("{identity:?}").contains("secret.owner@example.com"));
    for rendered in [format!("{error}"), format!("{error:?}")] {
        for secret in [
            "secret.owner@example.com",
            "header.payload.signature",
            "http://127.0.0.1:1234/cdn-cgi/access/certs",
        ] {
            assert!(!rendered.contains(secret));
        }
    }
    assert_eq!(error.classification(), AccessFailure::Unauthenticated);
}
