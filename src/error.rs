//! Application errors.

use axum::{
    Json,
    http::{HeaderValue, StatusCode, header::CONTENT_RANGE},
    response::{IntoResponse, Response},
};
use serde::Serialize;

use crate::app::RequestId;

#[derive(Debug)]
pub struct AppError {
    request_id: RequestId,
    kind: ErrorKind,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum SafeErrorDetails {
    ExpectedOffset {
        #[serde(rename = "expectedOffset", serialize_with = "serialize_decimal_u64")]
        expected_offset: u64,
    },
}

impl SafeErrorDetails {
    pub fn expected_offset(expected_offset: u64) -> Self {
        Self::ExpectedOffset { expected_offset }
    }
}

fn serialize_decimal_u64<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&value.to_string())
}

#[derive(Debug)]
enum ErrorKind {
    BadRequest {
        code: &'static str,
        message: &'static str,
    },
    Unauthorized,
    Forbidden,
    NotFound {
        code: &'static str,
        message: &'static str,
    },
    Conflict {
        code: &'static str,
        message: &'static str,
        details: Option<SafeErrorDetails>,
    },
    PayloadTooLarge,
    RangeNotSatisfiable {
        size: u64,
    },
    ServiceUnavailable {
        code: &'static str,
        message: &'static str,
    },
    InsufficientStorage,
    Internal,
}

impl AppError {
    pub fn unauthorized(request_id: RequestId) -> Self {
        Self {
            request_id,
            kind: ErrorKind::Unauthorized,
        }
    }

    pub fn forbidden(request_id: RequestId) -> Self {
        Self {
            request_id,
            kind: ErrorKind::Forbidden,
        }
    }

    pub fn bad_request(request_id: RequestId, code: &'static str, message: &'static str) -> Self {
        Self {
            request_id,
            kind: ErrorKind::BadRequest { code, message },
        }
    }

    pub fn not_found(request_id: RequestId, code: &'static str, message: &'static str) -> Self {
        Self {
            request_id,
            kind: ErrorKind::NotFound { code, message },
        }
    }

    pub fn conflict(
        request_id: RequestId,
        code: &'static str,
        message: &'static str,
        details: Option<SafeErrorDetails>,
    ) -> Self {
        Self {
            request_id,
            kind: ErrorKind::Conflict {
                code,
                message,
                details,
            },
        }
    }

    pub fn payload_too_large(request_id: RequestId) -> Self {
        Self {
            request_id,
            kind: ErrorKind::PayloadTooLarge,
        }
    }

    pub fn range_not_satisfiable(request_id: RequestId, size: u64) -> Self {
        Self {
            request_id,
            kind: ErrorKind::RangeNotSatisfiable { size },
        }
    }

    pub fn service_unavailable(
        request_id: RequestId,
        code: &'static str,
        message: &'static str,
    ) -> Self {
        Self {
            request_id,
            kind: ErrorKind::ServiceUnavailable { code, message },
        }
    }

    pub fn insufficient_storage(request_id: RequestId) -> Self {
        Self {
            request_id,
            kind: ErrorKind::InsufficientStorage,
        }
    }

    pub fn internal(request_id: RequestId) -> Self {
        Self {
            request_id,
            kind: ErrorKind::Internal,
        }
    }
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    error: ErrorBody<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorBody<'a> {
    code: &'a str,
    message: &'a str,
    request_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<&'a SafeErrorDetails>,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code, message, details, content_range_size) = match &self.kind {
            ErrorKind::BadRequest { code, message } => {
                (StatusCode::BAD_REQUEST, *code, *message, None, None)
            }
            ErrorKind::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "Authentication is required.",
                None,
                None,
            ),
            ErrorKind::Forbidden => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "You do not have permission to perform this action.",
                None,
                None,
            ),
            ErrorKind::NotFound { code, message } => {
                (StatusCode::NOT_FOUND, *code, *message, None, None)
            }
            ErrorKind::Conflict {
                code,
                message,
                details,
            } => (
                StatusCode::CONFLICT,
                *code,
                *message,
                details.as_ref(),
                None,
            ),
            ErrorKind::PayloadTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "The request payload is too large.",
                None,
                None,
            ),
            ErrorKind::RangeNotSatisfiable { size } => (
                StatusCode::RANGE_NOT_SATISFIABLE,
                "range_not_satisfiable",
                "The requested range is not satisfiable.",
                None,
                Some(*size),
            ),
            ErrorKind::ServiceUnavailable { code, message } => {
                (StatusCode::SERVICE_UNAVAILABLE, *code, *message, None, None)
            }
            ErrorKind::InsufficientStorage => (
                StatusCode::INSUFFICIENT_STORAGE,
                "insufficient_storage",
                "There is not enough storage to complete the request.",
                None,
                None,
            ),
            ErrorKind::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "An internal error occurred.",
                None,
                None,
            ),
        };
        let mut response = (
            status,
            Json(ErrorEnvelope {
                error: ErrorBody {
                    code,
                    message,
                    request_id: self.request_id.as_str(),
                    details,
                },
            }),
        )
            .into_response();
        if let Some(size) = content_range_size {
            response.headers_mut().insert(
                CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{size}"))
                    .expect("a u64 produces a valid Content-Range header"),
            );
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        body::to_bytes,
        http::{
            StatusCode,
            header::{CONTENT_RANGE, CONTENT_TYPE},
        },
        response::IntoResponse,
    };
    use serde_json::json;
    use uuid::Uuid;

    use super::{AppError, SafeErrorDetails};
    use crate::app::RequestId;

    fn request_id() -> RequestId {
        RequestId::from_uuid(Uuid::parse_str("0198f67e-9c0b-7000-8000-000000000001").unwrap())
    }

    #[tokio::test]
    async fn serializes_bad_request_exactly_without_details() {
        let response =
            AppError::bad_request(request_id(), "invalid_request", "The request is invalid.")
                .into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            std::str::from_utf8(&body).unwrap(),
            r#"{"error":{"code":"invalid_request","message":"The request is invalid.","requestId":"0198f67e-9c0b-7000-8000-000000000001"}}"#
        );
    }

    #[tokio::test]
    async fn maps_unauthorized_to_safe_fixed_response() {
        let response = AppError::unauthorized(request_id()).into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            json!({"error": {
                "code": "unauthorized",
                "message": "Authentication is required.",
                "requestId": "0198f67e-9c0b-7000-8000-000000000001"
            }})
        );
    }

    #[tokio::test]
    async fn maps_forbidden_to_safe_fixed_response() {
        let response = AppError::forbidden(request_id()).into_response();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            json!({"error": {
                "code": "forbidden", "message": "You do not have permission to perform this action.",
                "requestId": "0198f67e-9c0b-7000-8000-000000000001"
            }})
        );
    }

    #[tokio::test]
    async fn maps_not_found_with_caller_safe_fields() {
        let response = AppError::not_found(
            request_id(),
            "project_not_found",
            "The project was not found.",
        )
        .into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            json!({"error": {
                "code": "project_not_found", "message": "The project was not found.",
                "requestId": "0198f67e-9c0b-7000-8000-000000000001"
            }})
        );
    }

    #[tokio::test]
    async fn maps_conflict_with_typed_expected_offset_details() {
        let response = AppError::conflict(
            request_id(),
            "upload_offset_conflict",
            "The upload offset does not match the committed offset.",
            Some(SafeErrorDetails::expected_offset(33_554_432)),
        )
        .into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            json!({"error": {
                "code": "upload_offset_conflict",
                "message": "The upload offset does not match the committed offset.",
                "requestId": "0198f67e-9c0b-7000-8000-000000000001",
                "details": {"expectedOffset": "33554432"}
            }})
        );
    }

    #[tokio::test]
    async fn conflict_without_details_omits_details_field() {
        let response = AppError::conflict(
            request_id(),
            "project_conflict",
            "The project conflicts with existing state.",
            None,
        )
        .into_response();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = serde_json::from_slice::<serde_json::Value>(&body).unwrap();

        assert!(body["error"].get("details").is_none());
    }

    #[tokio::test]
    async fn maps_payload_too_large_to_safe_fixed_response() {
        let response = AppError::payload_too_large(request_id()).into_response();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            json!({"error": {
                "code": "payload_too_large", "message": "The request payload is too large.",
                "requestId": "0198f67e-9c0b-7000-8000-000000000001"
            }})
        );
    }

    #[tokio::test]
    async fn maps_unsatisfiable_range_with_known_size_header() {
        let response = AppError::range_not_satisfiable(request_id(), 8192).into_response();
        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(
            response.headers().get(CONTENT_RANGE).unwrap(),
            "bytes */8192"
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            json!({"error": {
                "code": "range_not_satisfiable", "message": "The requested range is not satisfiable.",
                "requestId": "0198f67e-9c0b-7000-8000-000000000001"
            }})
        );
    }

    #[tokio::test]
    async fn maps_service_unavailable_with_caller_safe_fields() {
        let response = AppError::service_unavailable(
            request_id(),
            "database_unavailable",
            "The service is temporarily unavailable.",
        )
        .into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            json!({"error": {
                "code": "database_unavailable", "message": "The service is temporarily unavailable.",
                "requestId": "0198f67e-9c0b-7000-8000-000000000001"
            }})
        );
    }

    #[tokio::test]
    async fn maps_insufficient_storage_to_safe_fixed_response() {
        let response = AppError::insufficient_storage(request_id()).into_response();
        assert_eq!(response.status(), StatusCode::INSUFFICIENT_STORAGE);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            json!({"error": {
                "code": "insufficient_storage", "message": "There is not enough storage to complete the request.",
                "requestId": "0198f67e-9c0b-7000-8000-000000000001"
            }})
        );
    }

    #[tokio::test]
    async fn maps_internal_error_to_safe_fixed_response() {
        let response = AppError::internal(request_id()).into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            json!({"error": {
                "code": "internal_error", "message": "An internal error occurred.",
                "requestId": "0198f67e-9c0b-7000-8000-000000000001"
            }})
        );
    }
}
