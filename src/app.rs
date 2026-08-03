//! Application assembly.

use std::{fmt, sync::Arc};

use axum::{
    Router,
    extract::Request,
    http::{HeaderName, HeaderValue},
    middleware::{self, Next},
    response::Response,
};
use uuid::{Uuid, Version};

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
    use crate::error::AppError;

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
