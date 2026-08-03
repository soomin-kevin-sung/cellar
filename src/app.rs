//! Application assembly.

use std::{fmt, sync::Arc, time::Duration};

use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderName, HeaderValue, Method, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use tower_http::trace::TraceLayer;
use uuid::{Uuid, Version};

use crate::{
    auth::{AccessFailure, AccessVerifier},
    config::CanonicalOrigin,
    error::AppError,
    projects::{ProjectService, project_router},
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
struct ApiSecurityState {
    verifier: Arc<dyn AccessVerifier>,
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
    let state = ApiSecurityState {
        verifier,
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
            state.clone(),
            origin_middleware,
        ))
        .layer(middleware::from_fn_with_state(state, access_middleware))
        .layer(trace);
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

async fn access_middleware(
    State(state): State<ApiSecurityState>,
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
    State(state): State<ApiSecurityState>,
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
    use axum::{
        Extension, Router,
        body::{Body, to_bytes},
        http::{Request, StatusCode, header::HeaderName},
        routing::get,
    };
    use serde_json::Value;
    use tower::ServiceExt;
    use uuid::{Uuid, Version};

    use super::{RequestId, X_REQUEST_ID, with_request_ids};
    use crate::{auth::AccessFailure, error::AppError};

    use super::{RejectionReason, rejection_reason};

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
