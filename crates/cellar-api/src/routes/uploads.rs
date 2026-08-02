use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::extract::{Extension, Request};
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post, put};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use cellar_auth::EnrollmentStore;
use cellar_core::{
    DEFAULT_MAX_CHUNK_SIZE, FileEntryId, NewUpload, OperationId, ProjectId, UploadId,
    UploadService, UploadServiceError, UploadSession,
};
use http_body_util::BodyExt as _;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

use super::session::{SessionState, require_json_content_type};

pub const MAX_UPLOAD_CHUNK_BYTES: i64 = DEFAULT_MAX_CHUNK_SIZE;
pub const MAX_UPLOAD_JSON_BYTES: usize = 16 * 1024;
pub const DEFAULT_UPLOAD_BODY_IDLE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(10 * 60);

const REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");
const UPLOAD_OFFSET: HeaderName = HeaderName::from_static("upload-offset");
const DIGEST: HeaderName = HeaderName::from_static("digest");
const MAX_REQUEST_ID_BYTES: usize = 128;

type Clock = Arc<dyn Fn() -> OffsetDateTime + Send + Sync>;

#[derive(Clone)]
struct UploadsState {
    service: UploadService,
    clock: Clock,
    body_idle_timeout: std::time::Duration,
}

pub fn uploads_router<S>(service: UploadService) -> Router<SessionState<S>>
where
    S: EnrollmentStore + 'static,
{
    uploads_router_with_clock(service, OffsetDateTime::now_utc)
}

pub fn uploads_router_with_clock<S, F>(service: UploadService, clock: F) -> Router<SessionState<S>>
where
    S: EnrollmentStore + 'static,
    F: Fn() -> OffsetDateTime + Send + Sync + 'static,
{
    uploads_router_with_clock_and_idle_timeout(service, clock, DEFAULT_UPLOAD_BODY_IDLE_TIMEOUT)
}

pub fn uploads_router_with_clock_and_idle_timeout<S, F>(
    service: UploadService,
    clock: F,
    body_idle_timeout: std::time::Duration,
) -> Router<SessionState<S>>
where
    S: EnrollmentStore + 'static,
    F: Fn() -> OffsetDateTime + Send + Sync + 'static,
{
    let state = UploadsState {
        service,
        clock: Arc::new(clock),
        body_idle_timeout,
    };
    Router::new()
        .route(
            "/api/v1/uploads",
            post(create_upload).fallback(method_not_allowed),
        )
        .route(
            "/api/v1/uploads/{id}",
            get(upload_status)
                .delete(cancel_upload)
                .fallback(method_not_allowed),
        )
        .route(
            "/api/v1/uploads/{id}/chunk",
            put(put_chunk).fallback(method_not_allowed),
        )
        .route(
            "/api/v1/uploads/{id}/finalize",
            post(finalize_upload).fallback(method_not_allowed),
        )
        .route("/api/v1/uploads/", any(invalid_path))
        .route("/api/v1/uploads/{id}/", any(invalid_path))
        .route("/api/v1/uploads/{id}/{extra}", any(invalid_path))
        .route("/api/v1/uploads/{id}/{extra}/{*rest}", any(invalid_path))
        .layer(Extension(state))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateUploadBody {
    project_id: String,
    destination_parent_id: Option<String>,
    destination_name: String,
    expected_size: String,
    expected_hash: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UploadBody {
    id: String,
    project_id: String,
    destination_parent_id: Option<String>,
    destination_name: String,
    expected_size: String,
    committed_offset: String,
    max_chunk_size: String,
    expires_at: String,
}

impl UploadBody {
    fn from_session(session: UploadSession, max_chunk_size: i64) -> Result<Self, UploadApiError> {
        Ok(Self {
            id: session.id.to_string(),
            project_id: session.project_id.to_string(),
            destination_parent_id: session.destination_parent_id.map(|id| id.to_string()),
            destination_name: session.destination_name,
            expected_size: session.expected_size.to_string(),
            committed_offset: session.committed_offset.to_string(),
            max_chunk_size: max_chunk_size.to_string(),
            expires_at: session
                .expires_at
                .to_offset(UtcOffset::UTC)
                .format(&Rfc3339)
                .map_err(|_| UploadApiError::bare_unavailable())?,
        })
    }
}

async fn create_upload(
    Extension(state): Extension<UploadsState>,
    request: Request,
) -> Result<impl IntoResponse, UploadApiError> {
    let (parts, body) = request.into_parts();
    let request_id = request_id(&parts.headers)?;
    require_json(&parts.headers, &request_id)?;
    let input: CreateUploadBody = parse_json(body, &request_id).await?;
    let project_id: ProjectId = input
        .project_id
        .parse()
        .map_err(|_| UploadApiError::invalid("invalid_project_id", request_id.clone()))?;
    if project_id.to_string() != input.project_id {
        return Err(UploadApiError::invalid("invalid_project_id", request_id));
    }
    let destination_parent_id: Option<FileEntryId> = input
        .destination_parent_id
        .as_deref()
        .map(str::parse)
        .transpose()
        .map_err(|_| {
            UploadApiError::invalid("invalid_destination_parent_id", request_id.clone())
        })?;
    if destination_parent_id
        .zip(input.destination_parent_id.as_deref())
        .is_some_and(|(parsed, raw)| parsed.to_string() != raw)
    {
        return Err(UploadApiError::invalid(
            "invalid_destination_parent_id",
            request_id,
        ));
    }
    let expected_size = parse_decimal_i64(&input.expected_size)
        .ok_or_else(|| UploadApiError::invalid("invalid_expected_size", request_id.clone()))?;
    let expected_hash = input
        .expected_hash
        .as_deref()
        .map(parse_base64_digest)
        .transpose()
        .map_err(|_| UploadApiError::invalid("invalid_expected_hash", request_id.clone()))?;
    let session = state
        .service
        .create(
            NewUpload {
                project_id,
                destination_parent_id,
                destination_name: input.destination_name,
                expected_size,
                expected_hash,
            },
            (state.clock)(),
        )
        .await
        .map_err(|error| UploadApiError::service(error, request_id.clone()))?;
    let offset = session.committed_offset.to_string();
    let body = UploadBody::from_session(session, state.service.limits().max_chunk_size)
        .map_err(|_| UploadApiError::unavailable(request_id.clone()))?;
    Ok((
        StatusCode::CREATED,
        [(REQUEST_ID, request_id), (UPLOAD_OFFSET, offset)],
        Json(body),
    ))
}

async fn upload_status(
    Extension(state): Extension<UploadsState>,
    request: Request,
) -> Result<impl IntoResponse, UploadApiError> {
    let request_id = request_id(request.headers())?;
    let id = parse_upload_path(request.uri().path(), None, &request_id)?;
    let session = state
        .service
        .status(id, (state.clock)())
        .await
        .map_err(|error| UploadApiError::service(error, request_id.clone()))?;
    let offset = session.committed_offset.to_string();
    let body = UploadBody::from_session(session, state.service.limits().max_chunk_size)
        .map_err(|_| UploadApiError::unavailable(request_id.clone()))?;
    Ok((
        [(REQUEST_ID, request_id), (UPLOAD_OFFSET, offset)],
        Json(body),
    ))
}

async fn put_chunk(
    Extension(state): Extension<UploadsState>,
    request: Request,
) -> Result<impl IntoResponse, UploadApiError> {
    let (parts, body) = request.into_parts();
    let request_id = request_id(&parts.headers)?;
    let id = parse_upload_path(parts.uri.path(), Some("chunk"), &request_id)?;
    require_octet_stream(&parts.headers, &request_id)?;
    let offset = one_header(&parts.headers, &UPLOAD_OFFSET)
        .and_then(parse_decimal_i64)
        .ok_or_else(|| UploadApiError::invalid("invalid_upload_offset", request_id.clone()))?;
    let digest = one_header(&parts.headers, &DIGEST)
        .and_then(|value| value.strip_prefix("sha-256="))
        .ok_or_else(|| UploadApiError::invalid("invalid_digest", request_id.clone()))
        .and_then(|value| {
            parse_base64_digest(value)
                .map_err(|_| UploadApiError::invalid("invalid_digest", request_id.clone()))
        })?;
    let content_length = one_header(&parts.headers, &header::CONTENT_LENGTH)
        .and_then(parse_decimal_i64)
        .ok_or_else(|| UploadApiError::invalid("invalid_content_length", request_id.clone()))?;
    let max_chunk = state.service.limits().max_chunk_size;
    if content_length > max_chunk {
        return Err(UploadApiError::too_large(request_id));
    }
    let max_chunk =
        usize::try_from(max_chunk).map_err(|_| UploadApiError::unavailable(request_id.clone()))?;
    let bytes =
        read_chunk_body(body, max_chunk, state.body_idle_timeout, request_id.clone()).await?;
    if i64::try_from(bytes.len()).ok() != Some(content_length) {
        return Err(UploadApiError::invalid(
            "content_length_mismatch",
            request_id,
        ));
    }
    let session = state
        .service
        .put_chunk(id, offset, &bytes, digest, (state.clock)())
        .await
        .map_err(|error| UploadApiError::service(error, request_id.clone()))?;
    Ok((
        StatusCode::NO_CONTENT,
        [
            (REQUEST_ID, request_id),
            (UPLOAD_OFFSET, session.committed_offset.to_string()),
        ],
    ))
}

async fn read_chunk_body(
    mut body: Body,
    max_bytes: usize,
    idle_timeout: std::time::Duration,
    request_id: String,
) -> Result<Vec<u8>, UploadApiError> {
    let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
    let mut deadline = tokio::time::Instant::now() + idle_timeout;
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(UploadApiError::body_timeout(request_id));
        }
        let frame = tokio::time::timeout_at(deadline, body.frame())
            .await
            .map_err(|_| UploadApiError::body_timeout(request_id.clone()))?;
        let Some(frame) = frame else {
            break;
        };
        let frame = frame
            .map_err(|_| UploadApiError::invalid("invalid_upload_body", request_id.clone()))?;
        let data = frame
            .into_data()
            .map_err(|_| UploadApiError::invalid("invalid_upload_body", request_id.clone()))?;
        if data.is_empty() {
            tokio::task::yield_now().await;
            if tokio::time::Instant::now() >= deadline {
                return Err(UploadApiError::body_timeout(request_id));
            }
            continue;
        }
        if bytes
            .len()
            .checked_add(data.len())
            .is_none_or(|length| length > max_bytes)
        {
            return Err(UploadApiError::too_large(request_id));
        }
        bytes.extend_from_slice(&data);
        deadline = tokio::time::Instant::now() + idle_timeout;
    }
    Ok(bytes)
}

async fn cancel_upload(
    Extension(state): Extension<UploadsState>,
    request: Request,
) -> Result<impl IntoResponse, UploadApiError> {
    let request_id = request_id(request.headers())?;
    let id = parse_upload_path(request.uri().path(), None, &request_id)?;
    state
        .service
        .cancel(id)
        .await
        .map_err(|error| UploadApiError::service(error, request_id.clone()))?;
    Ok((StatusCode::NO_CONTENT, [(REQUEST_ID, request_id)]))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FinalizedUploadBody {
    file_id: String,
    project_id: String,
    destination_parent_id: Option<String>,
    name: String,
    size: String,
    sha256: String,
}

async fn finalize_upload(
    Extension(state): Extension<UploadsState>,
    request: Request,
) -> Result<impl IntoResponse, UploadApiError> {
    let request_id = request_id(request.headers())?;
    let id = parse_upload_path(request.uri().path(), Some("finalize"), &request_id)?;
    let entry = state
        .service
        .finalize(id, (state.clock)())
        .await
        .map_err(|error| UploadApiError::service(error, request_id.clone()))?;
    let digest = entry
        .hash
        .ok_or_else(|| UploadApiError::unavailable(request_id.clone()))?;
    let body = FinalizedUploadBody {
        file_id: entry.id.to_string(),
        project_id: entry.project_id.to_string(),
        destination_parent_id: entry.parent_id.map(|parent| parent.to_string()),
        name: entry.exact_name.as_str().to_owned(),
        size: entry.size.to_string(),
        sha256: STANDARD.encode(digest),
    };
    Ok((StatusCode::OK, [(REQUEST_ID, request_id)], Json(body)))
}

async fn parse_json<T: DeserializeOwned>(
    body: Body,
    request_id: &str,
) -> Result<T, UploadApiError> {
    let bytes = to_bytes(body, MAX_UPLOAD_JSON_BYTES + 1)
        .await
        .map_err(|_| UploadApiError::too_large(request_id.to_owned()))?;
    if bytes.len() > MAX_UPLOAD_JSON_BYTES {
        return Err(UploadApiError::too_large(request_id.to_owned()));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| UploadApiError::invalid("invalid_upload_json", request_id.to_owned()))
}

fn require_json(headers: &HeaderMap, request_id: &str) -> Result<(), UploadApiError> {
    require_json_content_type(
        headers
            .get_all(header::CONTENT_TYPE)
            .iter()
            .map(|value| value.as_bytes()),
    )
    .map_err(|_| UploadApiError::unsupported_media_type(request_id.to_owned()))
}

fn require_octet_stream(headers: &HeaderMap, request_id: &str) -> Result<(), UploadApiError> {
    if one_header(headers, &header::CONTENT_TYPE) == Some("application/octet-stream") {
        Ok(())
    } else {
        Err(UploadApiError::unsupported_media_type(
            request_id.to_owned(),
        ))
    }
}

fn one_header<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        None
    } else {
        Some(value)
    }
}

fn parse_decimal_i64(value: &str) -> Option<i64> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return None;
    }
    value.parse().ok()
}

fn parse_base64_digest(value: &str) -> Result<[u8; 32], ()> {
    STANDARD
        .decode(value)
        .map_err(|_| ())?
        .try_into()
        .map_err(|_| ())
}

fn parse_upload_path(
    path: &str,
    subresource: Option<&str>,
    request_id: &str,
) -> Result<UploadId, UploadApiError> {
    let id = path
        .strip_prefix("/api/v1/uploads/")
        .and_then(|tail| {
            subresource.map_or(Some(tail), |subresource| {
                tail.strip_suffix(&format!("/{subresource}"))
            })
        })
        .filter(|id| !id.is_empty() && !id.contains(['/', '\\', '%']))
        .ok_or_else(|| UploadApiError::invalid("invalid_upload_id", request_id.to_owned()))?;
    let parsed: UploadId = id
        .parse()
        .map_err(|_| UploadApiError::invalid("invalid_upload_id", request_id.to_owned()))?;
    if parsed.to_string() != id {
        return Err(UploadApiError::invalid(
            "invalid_upload_id",
            request_id.to_owned(),
        ));
    }
    Ok(parsed)
}

fn request_id(headers: &HeaderMap) -> Result<String, UploadApiError> {
    match one_header(headers, &REQUEST_ID) {
        None if headers.get_all(&REQUEST_ID).iter().next().is_none() => {
            Ok(OperationId::new().to_string())
        }
        Some(value)
            if !value.is_empty()
                && value.len() <= MAX_REQUEST_ID_BYTES
                && value.bytes().all(|byte| byte.is_ascii_graphic()) =>
        {
            Ok(value.to_owned())
        }
        _ => Err(UploadApiError::invalid(
            "invalid_request_id",
            OperationId::new().to_string(),
        )),
    }
}

async fn invalid_path(headers: HeaderMap) -> UploadApiError {
    match request_id(&headers) {
        Ok(id) => UploadApiError::invalid("invalid_upload_id", id),
        Err(error) => error,
    }
}

async fn method_not_allowed(headers: HeaderMap) -> UploadApiError {
    match request_id(&headers) {
        Ok(id) => UploadApiError::method_not_allowed(id),
        Err(error) => error,
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorEnvelope {
    code: &'static str,
    message: &'static str,
    request_id: String,
    details: BTreeMap<String, String>,
}

pub struct UploadApiError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    request_id: String,
}

impl UploadApiError {
    fn invalid(code: &'static str, request_id: String) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code,
            message: "The upload request is invalid.",
            request_id,
        }
    }

    fn service(error: UploadServiceError, request_id: String) -> Self {
        let (status, message) = match error {
            UploadServiceError::Invalid => {
                (StatusCode::BAD_REQUEST, "The upload request is invalid.")
            }
            UploadServiceError::NotFound => {
                (StatusCode::NOT_FOUND, "The upload session was not found.")
            }
            UploadServiceError::Conflict => (
                StatusCode::CONFLICT,
                "The upload offset or content conflicts with durable state.",
            ),
            UploadServiceError::Expired => (StatusCode::GONE, "The upload session has expired."),
            UploadServiceError::TooManyRequests => (
                StatusCode::TOO_MANY_REQUESTS,
                "Upload capacity is currently exhausted.",
            ),
            UploadServiceError::InsufficientStorage => (
                StatusCode::INSUFFICIENT_STORAGE,
                "Insufficient storage is available for the upload.",
            ),
            UploadServiceError::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "The upload service is temporarily unavailable.",
            ),
        };
        Self {
            status,
            code: error.code(),
            message,
            request_id,
        }
    }

    fn too_large(request_id: String) -> Self {
        Self {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            code: "upload_chunk_too_large",
            message: "The upload body exceeds the allowed size.",
            request_id,
        }
    }

    fn unsupported_media_type(request_id: String) -> Self {
        Self {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            code: "unsupported_media_type",
            message: "The upload media type is not supported.",
            request_id,
        }
    }

    fn body_timeout(request_id: String) -> Self {
        Self {
            status: StatusCode::REQUEST_TIMEOUT,
            code: "upload_body_timeout",
            message: "The upload body stopped making progress.",
            request_id,
        }
    }

    fn unavailable(request_id: String) -> Self {
        Self::service(UploadServiceError::Unavailable, request_id)
    }

    fn bare_unavailable() -> Self {
        Self::unavailable(OperationId::new().to_string())
    }

    fn method_not_allowed(request_id: String) -> Self {
        Self {
            status: StatusCode::METHOD_NOT_ALLOWED,
            code: "method_not_allowed",
            message: "The request method is not allowed for this upload resource.",
            request_id,
        }
    }
}

impl fmt::Debug for UploadApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UploadApiError")
            .field("status", &self.status)
            .field("code", &self.code)
            .field("request_id", &"<redacted>")
            .finish()
    }
}

impl IntoResponse for UploadApiError {
    fn into_response(self) -> Response {
        let status = self.status;
        let body = ErrorEnvelope {
            code: self.code,
            message: self.message,
            request_id: self.request_id.clone(),
            details: BTreeMap::new(),
        };
        let mut response = (status, [(REQUEST_ID, self.request_id)], Json(body)).into_response();
        if status == StatusCode::TOO_MANY_REQUESTS {
            response.headers_mut().insert(
                header::RETRY_AFTER,
                "5".parse().expect("static header value"),
            );
        }
        response
    }
}
