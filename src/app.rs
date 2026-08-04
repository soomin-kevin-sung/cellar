//! Application assembly.

use std::{fmt, sync::Arc, time::Duration};

use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderName, HeaderValue, Method, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::any,
};
use tower_http::trace::TraceLayer;
use uuid::{Uuid, Version};

use crate::{
    accounts::{self, AccountService},
    auth::{AccessFailure, AccessVerifier},
    config::CanonicalOrigin,
    db::Database,
    error::AppError,
    files::{FileService, file_router},
    projects::{ProjectService, project_router},
    storage::Storage,
    uploads::{UploadService, upload_router},
    web::{WebBuildError, web_router, with_security_headers},
};

pub const X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestId(Arc<str>);

impl RequestId {
    pub(crate) fn from_uuid(value: Uuid) -> Self {
        assert_eq!(
            value.get_version(),
            Some(Version::SortRand),
            "request IDs must be UUIDv7"
        );
        Self(value.to_string().into())
    }

    fn generate() -> Self {
        Self::from_uuid(Uuid::now_v7())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

pub fn with_request_ids(router: Router) -> Router {
    router.layer(middleware::from_fn(request_id_middleware))
}

#[derive(Clone)]
struct AccessSecurityState {
    verifier: Arc<dyn AccessVerifier>,
}

#[derive(Clone)]
struct OriginSecurityState {
    external_origin: Arc<str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RejectionReason {
    MissingAssertion,
    InvalidAssertion,
    AuthorizationMismatch,
    JwksUnavailable,
    OriginRejected,
}

impl RejectionReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::MissingAssertion => "missing_assertion",
            Self::InvalidAssertion => "invalid_assertion",
            Self::AuthorizationMismatch => "authorization_mismatch",
            Self::JwksUnavailable => "jwks_unavailable",
            Self::OriginRejected => "origin_rejected",
        }
    }
}

const fn rejection_reason(failure: AccessFailure) -> RejectionReason {
    match failure {
        AccessFailure::Unauthenticated => RejectionReason::InvalidAssertion,
        AccessFailure::Forbidden => RejectionReason::AuthorizationMismatch,
        AccessFailure::Unavailable => RejectionReason::JwksUnavailable,
    }
}

fn record_access_rejection(reason: RejectionReason, request_id: &RequestId) {
    if reason == RejectionReason::JwksUnavailable {
        tracing::warn!(
            reason = reason.as_str(),
            request_id = %request_id,
            "access request rejected"
        );
    } else {
        tracing::debug!(
            reason = reason.as_str(),
            request_id = %request_id,
            "access request rejected"
        );
    }
}

/// Applies the complete security boundary to the provided API router only.
///
/// Route templates are not yet available at this assembly boundary, so tracing
/// intentionally records only method, status, and request ID. It never records
/// raw URIs, headers, query strings, bodies, tokens, or identities.
pub fn secure_api_router(
    router: Router,
    verifier: Arc<dyn AccessVerifier>,
    external_origin: &CanonicalOrigin,
) -> Router {
    let access_state = AccessSecurityState { verifier };
    let origin_state = OriginSecurityState {
        external_origin: Arc::from(external_origin.as_str()),
    };
    let trace = TraceLayer::new_for_http()
        .make_span_with(|request: &axum::http::Request<Body>| {
            let request_id = request
                .extensions()
                .get::<RequestId>()
                .map(RequestId::as_str)
                .unwrap_or("missing");
            tracing::info_span!(
                "api_request",
                method = %request.method(),
                request_id = %request_id
            )
        })
        .on_response(
            |response: &Response, _latency: Duration, span: &tracing::Span| {
                tracing::info!(parent: span, status = %response.status());
            },
        );

    let secured = router
        .layer(middleware::from_fn_with_state(
            origin_state,
            origin_middleware,
        ))
        .layer(middleware::from_fn_with_state(
            access_state,
            access_middleware,
        ))
        .layer(trace);
    with_request_ids(secured)
}

fn quick_api_router(router: Router, external_origin: &CanonicalOrigin) -> Router {
    let origin_state = OriginSecurityState {
        external_origin: Arc::from(external_origin.as_str()),
    };
    let secured = router.layer(middleware::from_fn_with_state(
        origin_state,
        origin_middleware,
    ));
    with_request_ids(secured)
}

fn session_api_router(
    router: Router,
    accounts: AccountService,
    external_origin: &CanonicalOrigin,
) -> Router {
    let origin_state = OriginSecurityState {
        external_origin: Arc::from(external_origin.as_str()),
    };
    let secured = router
        .layer(middleware::from_fn_with_state(
            origin_state,
            origin_middleware,
        ))
        .layer(middleware::from_fn_with_state(
            accounts,
            accounts::session_middleware,
        ));
    with_request_ids(secured)
}

/// Production composition point for the project API security boundary.
pub fn secure_project_api_router(
    service: ProjectService,
    verifier: Arc<dyn AccessVerifier>,
    external_origin: &CanonicalOrigin,
) -> Router {
    secure_api_router(project_router(service), verifier, external_origin)
}

/// Production composition point for all currently implemented API routes.
pub fn secure_cellar_api_router(
    database: Arc<Database>,
    storage: Arc<Storage>,
    verifier: Arc<dyn AccessVerifier>,
    external_origin: &CanonicalOrigin,
) -> Router {
    let projects = project_router(ProjectService::new(database.clone(), storage.clone()));
    let uploads = upload_router(UploadService::new(database.clone(), storage.clone()));
    let files = file_router(FileService::new(database, storage));
    secure_api_router(
        projects.merge(uploads).merge(files),
        verifier,
        external_origin,
    )
}

/// Builds the complete production application: a public embedded SPA and a
/// Cloudflare Access-protected API, including a JSON-only API fallback.
pub fn build_cellar_app(
    database: Arc<Database>,
    storage: Arc<Storage>,
    verifier: Arc<dyn AccessVerifier>,
    external_origin: &CanonicalOrigin,
) -> Result<Router, WebBuildError> {
    Ok(build_cellar_app_with_web(
        web_router()?,
        database,
        storage,
        verifier,
        external_origin,
    ))
}

/// Builds the application for a temporary Quick Tunnel with application
/// accounts and secure cookie-backed sessions.
pub fn build_cellar_quick_app(
    database: Arc<Database>,
    storage: Arc<Storage>,
    external_origin: &CanonicalOrigin,
    bootstrap_password: &str,
) -> Result<Router, WebBuildError> {
    let accounts = AccountService::new(database.clone(), Arc::<str>::from(bootstrap_password));
    let projects = project_router(ProjectService::new(database.clone(), storage.clone()));
    let uploads = upload_router(UploadService::new(database.clone(), storage.clone()));
    let files = file_router(FileService::new(database, storage));
    let public_auth = quick_api_router(accounts::public_router(accounts.clone()), external_origin);
    let api = session_api_router(
        projects
            .merge(uploads)
            .merge(files)
            .merge(accounts::protected_router(accounts.clone())),
        accounts.clone(),
        external_origin,
    );
    let api_fallback = session_api_router(
        Router::new()
            .route("/api", any(api_not_found))
            .route("/api/", any(api_not_found))
            .route("/api/{*path}", any(api_not_found)),
        accounts,
        external_origin,
    );

    Ok(with_security_headers(
        Router::new()
            .merge(public_auth)
            .merge(api)
            .merge(api_fallback)
            .merge(web_router()?),
    ))
}

fn build_cellar_app_with_web(
    web: Router,
    database: Arc<Database>,
    storage: Arc<Storage>,
    verifier: Arc<dyn AccessVerifier>,
    external_origin: &CanonicalOrigin,
) -> Router {
    let api = secure_cellar_api_router(database, storage, verifier.clone(), external_origin);
    let api_fallback = secure_api_router(
        Router::new()
            .route("/api", any(api_not_found))
            .route("/api/", any(api_not_found))
            .route("/api/{*path}", any(api_not_found)),
        verifier,
        external_origin,
    );

    with_security_headers(Router::new().merge(api).merge(api_fallback).merge(web))
}

async fn api_not_found(axum::Extension(request_id): axum::Extension<RequestId>) -> AppError {
    AppError::not_found(request_id, "api_not_found", "The API route was not found.")
}

/// Secures an upload router assembled with injected service dependencies.
pub fn secure_upload_api_router(
    service: UploadService,
    verifier: Arc<dyn AccessVerifier>,
    external_origin: &CanonicalOrigin,
) -> Router {
    secure_api_router(upload_router(service), verifier, external_origin)
}

async fn access_middleware(
    State(state): State<AccessSecurityState>,
    mut request: Request,
    next: Next,
) -> Response {
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .cloned()
        .expect("secure API router installs request IDs before authentication");
    let mut assertions = request.headers().get_all("Cf-Access-Jwt-Assertion").iter();
    let Some(assertion) = assertions.next() else {
        record_access_rejection(RejectionReason::MissingAssertion, &request_id);
        return AppError::unauthorized(request_id).into_response();
    };
    if assertions.next().is_some() {
        record_access_rejection(RejectionReason::InvalidAssertion, &request_id);
        return AppError::unauthorized(request_id).into_response();
    }
    let Ok(assertion) = assertion.to_str() else {
        record_access_rejection(RejectionReason::InvalidAssertion, &request_id);
        return AppError::unauthorized(request_id).into_response();
    };
    let identity = match state.verifier.verify(assertion).await {
        Ok(identity) => identity,
        Err(error) => {
            let classification = error.classification();
            record_access_rejection(rejection_reason(classification), &request_id);
            return match classification {
                AccessFailure::Unauthenticated => AppError::unauthorized(request_id),
                AccessFailure::Forbidden => AppError::forbidden(request_id),
                AccessFailure::Unavailable => AppError::service_unavailable(
                    request_id,
                    "authentication_unavailable",
                    "Authentication is temporarily unavailable.",
                ),
            }
            .into_response();
        }
    };

    request.extensions_mut().insert(identity);
    next.run(request).await
}

async fn origin_middleware(
    State(state): State<OriginSecurityState>,
    request: Request,
    next: Next,
) -> Response {
    if matches!(
        *request.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    ) {
        let request_id = request
            .extensions()
            .get::<RequestId>()
            .cloned()
            .expect("secure API router installs request IDs before origin checks");
        let mut origins = request.headers().get_all(header::ORIGIN).iter();
        let valid = origins.next().is_some_and(|origin| {
            origin.as_bytes() == state.external_origin.as_bytes() && origins.next().is_none()
        });
        if !valid {
            record_access_rejection(RejectionReason::OriginRejected, &request_id);
            return AppError::forbidden(request_id).into_response();
        }
    }

    next.run(request).await
}

async fn request_id_middleware(mut request: Request, next: Next) -> Response {
    let request_id = RequestId::generate();
    request.extensions_mut().insert(request_id.clone());

    let mut response = next.run(request).await;
    let header_value = HeaderValue::from_str(request_id.as_str())
        .expect("generated UUID request IDs are valid header values");
    response.headers_mut().insert(X_REQUEST_ID, header_value);
    response
}

#[cfg(test)]
mod tests {
    use std::{future::Future, pin::Pin, sync::Arc};

    use axum::{
        Extension, Router,
        body::{Body, to_bytes},
        http::{Method, Request, StatusCode, header, header::HeaderName},
        routing::get,
    };
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use tower::ServiceExt;
    use uuid::{Uuid, Version};

    use super::{RequestId, X_REQUEST_ID, build_cellar_app_with_web, with_request_ids};
    use crate::{
        auth::{AccessError, AccessFailure, AccessVerifier, OwnerIdentity},
        config::Config,
        db::Database,
        error::AppError,
        storage::Storage,
    };

    use super::{RejectionReason, rejection_reason};

    #[derive(Clone)]
    struct FakeAccessVerifier;

    impl AccessVerifier for FakeAccessVerifier {
        fn verify<'a>(
            &'a self,
            assertion: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<OwnerIdentity, AccessError>> + Send + 'a>> {
            Box::pin(async move {
                if assertion == "valid" {
                    OwnerIdentity::try_from_email("owner@example.com")
                } else {
                    Err(AccessError::unauthenticated())
                }
            })
        }
    }

    struct AppFixture {
        _temp: TempDir,
        database: Database,
        app: Router,
    }

    impl AppFixture {
        async fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            let storage = Arc::new(Storage::new(root.clone()).unwrap());
            storage.initialize().await.unwrap();
            let database = Database::open(root.join(".cellar/cellar.db"))
                .await
                .unwrap();
            let config = Config::parse(&format!(
                r#"bind = "127.0.0.1:8787"
external_origin = "https://files.example.com"
data_root = "{}"
database_path = "{}"

[access]
team_domain = "https://example.cloudflareaccess.com"
audience = "audience"
owner_email = "owner@example.com"
"#,
                root.to_string_lossy().replace('\\', "/"),
                root.join(".cellar/cellar.db")
                    .to_string_lossy()
                    .replace('\\', "/"),
            ))
            .unwrap();
            let web = Router::new().route("/", get(|| async { "fixture index" }));
            let app = build_cellar_app_with_web(
                web,
                Arc::new(database.clone()),
                storage,
                Arc::new(FakeAccessVerifier),
                config.external_origin(),
            );
            Self {
                _temp: temp,
                database,
                app,
            }
        }
    }

    fn api_request(method: Method, uri: &str) -> http::request::Builder {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("Cf-Access-Jwt-Assertion", "valid")
    }

    fn assert_security_headers(response: &axum::response::Response) {
        assert_eq!(
            response.headers()[header::CONTENT_SECURITY_POLICY],
            "default-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; connect-src 'self'; script-src 'self'; style-src 'self'"
        );
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        assert_eq!(response.headers()[header::REFERRER_POLICY], "no-referrer");
    }

    #[tokio::test]
    async fn production_composition_keeps_web_public_and_known_api_secured() {
        let fixture = AppFixture::new().await;
        let root = fixture
            .app
            .clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(root.status(), StatusCode::OK);
        assert_security_headers(&root);

        let unauthenticated = fixture
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/projects")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
        assert_security_headers(&unauthenticated);

        let no_origin_get = fixture
            .app
            .clone()
            .oneshot(
                api_request(Method::GET, "/api/v1/projects")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(no_origin_get.status(), StatusCode::OK);

        fixture.database.close().await;
    }

    #[tokio::test]
    async fn production_composition_enforces_exact_origin_on_known_writes() {
        let fixture = AppFixture::new().await;
        for origin in [
            None,
            Some("https://evil.example"),
            Some("https://files.example.com/"),
        ] {
            let mut request = api_request(Method::POST, "/api/v1/projects")
                .header(header::CONTENT_TYPE, "application/json");
            if let Some(origin) = origin {
                request = request.header(header::ORIGIN, origin);
            }
            let response = fixture
                .app
                .clone()
                .oneshot(request.body(Body::from(r#"{"name":"Project"}"#)).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert_security_headers(&response);
        }

        let accepted = fixture
            .app
            .clone()
            .oneshot(
                api_request(Method::POST, "/api/v1/projects")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ORIGIN, "https://files.example.com")
                    .body(Body::from(r#"{"name":"Project"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(accepted.status(), StatusCode::CREATED);
        fixture.database.close().await;
    }

    #[tokio::test]
    async fn unknown_api_is_secured_json_with_matching_request_id_never_spa() {
        let fixture = AppFixture::new().await;
        let unauthenticated = fixture
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/not-real")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

        let response = fixture
            .app
            .clone()
            .oneshot(
                api_request(Method::GET, "/api/not-real")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        assert_security_headers(&response);
        let request_id = response.headers()[&X_REQUEST_ID]
            .to_str()
            .unwrap()
            .to_owned();
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(
            body,
            json!({"error": {
                "code": "api_not_found",
                "message": "The API route was not found.",
                "requestId": request_id,
            }})
        );
        fixture.database.close().await;
    }

    #[tokio::test]
    async fn authenticated_unknown_api_unsafe_methods_require_origin_before_json_not_found() {
        let fixture = AppFixture::new().await;
        for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            let rejected = fixture
                .app
                .clone()
                .oneshot(
                    api_request(method.clone(), "/api/v2/not-real")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
            assert_security_headers(&rejected);

            let response = fixture
                .app
                .clone()
                .oneshot(
                    api_request(method, "/api/v2/not-real")
                        .header(header::ORIGIN, "https://files.example.com")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
            assert!(response.headers().contains_key(&X_REQUEST_ID));
            assert_security_headers(&response);
        }
        fixture.database.close().await;
    }

    #[tokio::test]
    async fn api_trailing_slash_stays_inside_auth_origin_and_json_fallback_boundary() {
        let fixture = AppFixture::new().await;

        let unauthenticated = fixture
            .app
            .clone()
            .oneshot(Request::builder().uri("/api/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            unauthenticated.headers()[header::CONTENT_TYPE],
            "application/json"
        );
        assert_security_headers(&unauthenticated);

        let authenticated = fixture
            .app
            .clone()
            .oneshot(
                api_request(Method::GET, "/api/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authenticated.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            authenticated.headers()[header::CONTENT_TYPE],
            "application/json"
        );
        assert_security_headers(&authenticated);
        let request_id = authenticated.headers()[&X_REQUEST_ID]
            .to_str()
            .unwrap()
            .to_owned();
        let body: Value = serde_json::from_slice(
            &to_bytes(authenticated.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["error"]["code"], "api_not_found");
        assert_eq!(body["error"]["requestId"], request_id);

        let missing_origin = fixture
            .app
            .clone()
            .oneshot(
                api_request(Method::POST, "/api/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing_origin.status(), StatusCode::FORBIDDEN);
        assert_security_headers(&missing_origin);

        let exact_origin = fixture
            .app
            .clone()
            .oneshot(
                api_request(Method::POST, "/api/")
                    .header(header::ORIGIN, "https://files.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(exact_origin.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            exact_origin.headers()[header::CONTENT_TYPE],
            "application/json"
        );
        assert_security_headers(&exact_origin);

        fixture.database.close().await;
    }

    #[test]
    fn authentication_log_reasons_are_closed_safe_literals() {
        assert_eq!(
            RejectionReason::MissingAssertion.as_str(),
            "missing_assertion"
        );
        assert_eq!(
            RejectionReason::InvalidAssertion.as_str(),
            "invalid_assertion"
        );
        assert_eq!(
            RejectionReason::AuthorizationMismatch.as_str(),
            "authorization_mismatch"
        );
        assert_eq!(
            RejectionReason::JwksUnavailable.as_str(),
            "jwks_unavailable"
        );
        assert_eq!(RejectionReason::OriginRejected.as_str(), "origin_rejected");
        assert_eq!(
            rejection_reason(AccessFailure::Unauthenticated),
            RejectionReason::InvalidAssertion
        );
        assert_eq!(
            rejection_reason(AccessFailure::Forbidden),
            RejectionReason::AuthorizationMismatch
        );
        assert_eq!(
            rejection_reason(AccessFailure::Unavailable),
            RejectionReason::JwksUnavailable
        );
    }

    #[tokio::test]
    async fn success_response_has_uuid_v7_request_id() {
        let app = with_request_ids(Router::new().route("/", get(|| async { "ok" })));
        let response = app.oneshot(Request::new(Body::empty())).await.unwrap();
        let value = response
            .headers()
            .get(HeaderName::from_static("x-request-id"))
            .unwrap()
            .to_str()
            .unwrap();
        let id = Uuid::parse_str(value).unwrap();

        assert_eq!(id.get_version(), Some(Version::SortRand));
    }

    #[tokio::test]
    async fn error_body_request_id_matches_response_header() {
        async fn handler(Extension(request_id): Extension<RequestId>) -> AppError {
            AppError::bad_request(request_id, "invalid_request", "The request is invalid.")
        }

        let app = with_request_ids(Router::new().route("/", get(handler)));
        let response = app.oneshot(Request::new(Body::empty())).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let header = response.headers().get(&X_REQUEST_ID).unwrap().clone();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(body["error"]["requestId"], header.to_str().unwrap());
    }

    #[tokio::test]
    async fn separate_requests_get_distinct_request_ids() {
        let app = with_request_ids(Router::new().route("/", get(|| async { "ok" })));
        let first = app
            .clone()
            .oneshot(Request::new(Body::empty()))
            .await
            .unwrap();
        let second = app.oneshot(Request::new(Body::empty())).await.unwrap();

        assert_ne!(
            first.headers().get(&X_REQUEST_ID),
            second.headers().get(&X_REQUEST_ID)
        );
    }

    #[test]
    #[should_panic(expected = "request IDs must be UUIDv7")]
    fn rejects_non_v7_request_id_construction() {
        RequestId::from_uuid(Uuid::nil());
    }

    #[tokio::test]
    async fn ignores_incoming_request_id_and_generates_uuid_v7() {
        let incoming = "0198f67e-9c0b-7000-8000-000000000001";
        let app = with_request_ids(Router::new().route("/", get(|| async { "ok" })));
        let request = Request::builder()
            .header(&X_REQUEST_ID, incoming)
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        let generated = response
            .headers()
            .get(&X_REQUEST_ID)
            .unwrap()
            .to_str()
            .unwrap();

        assert_ne!(generated, incoming);
        assert_eq!(
            Uuid::parse_str(generated).unwrap().get_version(),
            Some(Version::SortRand)
        );
    }
}
