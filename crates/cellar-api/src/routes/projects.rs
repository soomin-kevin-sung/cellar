use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::extract::{Extension, RawQuery, Request};
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use cellar_auth::EnrollmentStore;
use cellar_core::{
    MAX_PROJECT_LIST_LIMIT, NewProject, OperationId, Project, ProjectId, ProjectListFilter,
    ProjectPatch, ProjectService, ProjectServiceError, ProjectStatus,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

use super::session::{SessionState, require_json_content_type};

pub const MAX_PROJECT_JSON_BYTES: usize = 16 * 1024;
pub const MAX_PROJECT_BODY_BYTES: usize = MAX_PROJECT_JSON_BYTES;
pub const MAX_REQUEST_ID_BYTES: usize = 128;
pub const MAX_IDEMPOTENCY_KEY_BYTES: usize = 64;

const REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");
const IDEMPOTENCY_KEY: HeaderName = HeaderName::from_static("idempotency-key");

type Clock = Arc<dyn Fn() -> OffsetDateTime + Send + Sync>;

#[derive(Clone)]
struct ProjectsState {
    service: ProjectService,
    clock: Clock,
}

pub fn projects_router<S>(service: ProjectService) -> Router<SessionState<S>>
where
    S: EnrollmentStore + 'static,
{
    projects_router_with_clock(service, OffsetDateTime::now_utc)
}

pub fn projects_router_with_clock<S, F>(
    service: ProjectService,
    clock: F,
) -> Router<SessionState<S>>
where
    S: EnrollmentStore + 'static,
    F: Fn() -> OffsetDateTime + Send + Sync + 'static,
{
    let state = ProjectsState {
        service,
        clock: Arc::new(clock),
    };
    Router::new()
        .route(
            "/api/v1/projects",
            post(create_project)
                .get(list_projects)
                .fallback(method_not_allowed),
        )
        .route(
            "/api/v1/projects/{id}",
            get(get_project)
                .patch(update_project)
                .fallback(method_not_allowed),
        )
        .route(
            "/api/v1/projects/{id}/archive",
            post(archive_project).fallback(method_not_allowed),
        )
        .route("/api/v1/projects/", any(invalid_project_path))
        .route("/api/v1/projects/{id}/", any(invalid_project_path))
        .route("/api/v1/projects/{id}/{extra}", any(invalid_project_path))
        .route(
            "/api/v1/projects/{id}/{extra}/{*rest}",
            any(invalid_project_path),
        )
        .layer(Extension(state))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBody {
    name: String,
    #[serde(default)]
    description: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PatchBody {
    expected_version: String,
    name: Option<String>,
    description: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ArchiveBody {
    expected_version: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectBody {
    id: String,
    name: String,
    description: String,
    status: ProjectStatus,
    version: String,
    created_at: String,
    updated_at: String,
}

impl TryFrom<Project> for ProjectBody {
    type Error = ProjectApiError;

    fn try_from(project: Project) -> Result<Self, Self::Error> {
        Ok(Self {
            id: project.id.to_string(),
            name: project.name.as_str().to_owned(),
            description: project.description.as_str().to_owned(),
            status: project.status,
            version: project.version.to_string(),
            created_at: format_timestamp(project.created_at)?,
            updated_at: format_timestamp(project.updated_at)?,
        })
    }
}

async fn create_project(
    Extension(state): Extension<ProjectsState>,
    request: Request,
) -> Result<impl IntoResponse, ProjectApiError> {
    let (parts, body) = request.into_parts();
    let headers = parts.headers;
    let request_id = request_id(&headers)?;
    require_json(&headers, &request_id)?;
    let input: CreateBody = parse_body(body, &request_id).await?;
    let input = NewProject::try_new(input.name, input.description)
        .map_err(|error| ProjectApiError::invalid(error.code(), request_id.clone()))?;
    let idempotency_key = idempotency_key(&headers, &request_id)?;
    let result = state
        .service
        .create(input, idempotency_key, (state.clock)())
        .await
        .map_err(|error| ProjectApiError::service(error, request_id.clone()))?;
    let response = ProjectBody::try_from(result.project)?;
    Ok((
        StatusCode::CREATED,
        [(REQUEST_ID, request_id)],
        Json(response),
    ))
}

async fn list_projects(
    Extension(state): Extension<ProjectsState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<impl IntoResponse, ProjectApiError> {
    let request_id = request_id(&headers)?;
    let (filter, limit) = parse_list_query(query.as_deref(), &request_id)?;
    let projects = state
        .service
        .list(filter, limit)
        .await
        .map_err(|error| ProjectApiError::service(error, request_id.clone()))?;
    let projects = projects
        .into_iter()
        .map(ProjectBody::try_from)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(([(REQUEST_ID, request_id)], Json(projects)))
}

async fn get_project(
    Extension(state): Extension<ProjectsState>,
    request: Request,
) -> Result<impl IntoResponse, ProjectApiError> {
    let headers = request.headers();
    let request_id = request_id(headers)?;
    let id = parse_project_path(request.uri().path(), false, &request_id)?;
    let project = state
        .service
        .read(id)
        .await
        .map_err(|error| ProjectApiError::service(error, request_id.clone()))?;
    Ok((
        [(REQUEST_ID, request_id)],
        Json(ProjectBody::try_from(project)?),
    ))
}

async fn update_project(
    Extension(state): Extension<ProjectsState>,
    request: Request,
) -> Result<impl IntoResponse, ProjectApiError> {
    let (parts, body) = request.into_parts();
    let headers = parts.headers;
    let request_id = request_id(&headers)?;
    require_json(&headers, &request_id)?;
    let id = parse_project_path(parts.uri.path(), false, &request_id)?;
    let input: PatchBody = parse_body(body, &request_id).await?;
    let expected_version = parse_decimal_version(&input.expected_version)
        .map_err(|_| ProjectApiError::invalid("invalid_expected_version", request_id.clone()))?;
    let patch = ProjectPatch::try_new(input.name, input.description)
        .map_err(|error| ProjectApiError::invalid(error.code(), request_id.clone()))?;
    let project = state
        .service
        .update(id, expected_version, &patch, (state.clock)())
        .await
        .map_err(|error| ProjectApiError::service(error, request_id.clone()))?;
    Ok((
        [(REQUEST_ID, request_id)],
        Json(ProjectBody::try_from(project)?),
    ))
}

async fn archive_project(
    Extension(state): Extension<ProjectsState>,
    request: Request,
) -> Result<impl IntoResponse, ProjectApiError> {
    let (parts, body) = request.into_parts();
    let headers = parts.headers;
    let request_id = request_id(&headers)?;
    require_json(&headers, &request_id)?;
    let id = parse_project_path(parts.uri.path(), true, &request_id)?;
    let input: ArchiveBody = parse_body(body, &request_id).await?;
    let expected_version = parse_decimal_version(&input.expected_version)
        .map_err(|_| ProjectApiError::invalid("invalid_expected_version", request_id.clone()))?;
    let project = state
        .service
        .archive(id, expected_version, (state.clock)())
        .await
        .map_err(|error| ProjectApiError::service(error, request_id.clone()))?;
    Ok((
        [(REQUEST_ID, request_id)],
        Json(ProjectBody::try_from(project)?),
    ))
}

async fn parse_body<T: DeserializeOwned>(
    body: Body,
    request_id: &str,
) -> Result<T, ProjectApiError> {
    let body = to_bytes(body, MAX_PROJECT_JSON_BYTES + 1)
        .await
        .map_err(|_| ProjectApiError::invalid("request_body_too_large", request_id.to_owned()))?;
    if body.len() > MAX_PROJECT_JSON_BYTES {
        return Err(ProjectApiError::invalid(
            "request_body_too_large",
            request_id.to_owned(),
        ));
    }
    serde_json::from_slice(&body)
        .map_err(|_| ProjectApiError::invalid("invalid_project_json", request_id.to_owned()))
}

fn require_json(headers: &HeaderMap, request_id: &str) -> Result<(), ProjectApiError> {
    require_json_content_type(
        headers
            .get_all(header::CONTENT_TYPE)
            .iter()
            .map(|value| value.as_bytes()),
    )
    .map_err(|_| ProjectApiError::invalid("unsupported_media_type", request_id.to_owned()))
}

fn parse_project_path(
    path: &str,
    archive: bool,
    request_id: &str,
) -> Result<ProjectId, ProjectApiError> {
    let segment = path
        .strip_prefix("/api/v1/projects/")
        .and_then(|tail| {
            if archive {
                tail.strip_suffix("/archive")
            } else {
                Some(tail)
            }
        })
        .filter(|segment| !segment.is_empty() && !segment.contains('/') && !segment.contains('\\'))
        .ok_or_else(|| ProjectApiError::invalid("invalid_project_id", request_id.to_owned()))?;
    let decoded = percent_decode_segment(segment)
        .ok_or_else(|| ProjectApiError::invalid("invalid_project_id", request_id.to_owned()))?;
    decoded
        .parse()
        .map_err(|_| ProjectApiError::invalid("invalid_project_id", request_id.to_owned()))
}

fn percent_decode_segment(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex_value(*bytes.get(index + 1)?)?;
            let low = hex_value(*bytes.get(index + 2)?)?;
            let byte = (high << 4) | low;
            if matches!(byte, b'/' | b'\\') {
                return None;
            }
            decoded.push(byte);
            index += 3;
        } else {
            if matches!(bytes[index], b'/' | b'\\') {
                return None;
            }
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

const fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

async fn invalid_project_path(headers: HeaderMap) -> ProjectApiError {
    match request_id(&headers) {
        Ok(request_id) => ProjectApiError::invalid("invalid_project_id", request_id),
        Err(error) => error,
    }
}

async fn method_not_allowed(headers: HeaderMap) -> ProjectApiError {
    match request_id(&headers) {
        Ok(request_id) => ProjectApiError::method_not_allowed(request_id),
        Err(error) => error,
    }
}

fn request_id(headers: &HeaderMap) -> Result<String, ProjectApiError> {
    let values: Vec<_> = headers.get_all(&REQUEST_ID).iter().collect();
    if values.is_empty() {
        return Ok(OperationId::new().to_string());
    }
    if values.len() != 1 {
        return Err(ProjectApiError::invalid(
            "invalid_request_id",
            OperationId::new().to_string(),
        ));
    }
    let bytes = values[0].as_bytes();
    let value = std::str::from_utf8(bytes).map_err(|_| {
        ProjectApiError::invalid("invalid_request_id", OperationId::new().to_string())
    })?;
    if value.is_empty()
        || value.len() > MAX_REQUEST_ID_BYTES
        || !value.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(ProjectApiError::invalid(
            "invalid_request_id",
            OperationId::new().to_string(),
        ));
    }
    Ok(value.to_owned())
}

fn idempotency_key(
    headers: &HeaderMap,
    request_id: &str,
) -> Result<Option<OperationId>, ProjectApiError> {
    let values: Vec<_> = headers.get_all(&IDEMPOTENCY_KEY).iter().collect();
    if values.is_empty() {
        return Ok(None);
    }
    if values.len() != 1 || values[0].as_bytes().len() > MAX_IDEMPOTENCY_KEY_BYTES {
        return Err(ProjectApiError::invalid(
            "invalid_idempotency_key",
            request_id.to_owned(),
        ));
    }
    let value = values[0]
        .to_str()
        .map_err(|_| ProjectApiError::invalid("invalid_idempotency_key", request_id.to_owned()))?;
    value
        .parse()
        .map(Some)
        .map_err(|_| ProjectApiError::invalid("invalid_idempotency_key", request_id.to_owned()))
}

fn parse_list_query(
    query: Option<&str>,
    request_id: &str,
) -> Result<(ProjectListFilter, u32), ProjectApiError> {
    let mut status = None;
    let mut limit = None;
    if let Some(query) = query.filter(|query| !query.is_empty()) {
        for field in query.split('&') {
            let (key, value) = field.split_once('=').ok_or_else(|| {
                ProjectApiError::invalid("invalid_project_query", request_id.to_owned())
            })?;
            match key {
                "status" if status.is_none() => status = Some(value),
                "limit" if limit.is_none() => limit = Some(value),
                _ => {
                    return Err(ProjectApiError::invalid(
                        "invalid_project_query",
                        request_id.to_owned(),
                    ));
                }
            }
        }
    }
    let filter = match status.unwrap_or("active") {
        "all" => ProjectListFilter::All,
        "active" => ProjectListFilter::Status(ProjectStatus::Active),
        "archived" => ProjectListFilter::Status(ProjectStatus::Archived),
        _ => {
            return Err(ProjectApiError::invalid(
                "invalid_project_status",
                request_id.to_owned(),
            ));
        }
    };
    let limit = match limit {
        None => MAX_PROJECT_LIST_LIMIT,
        Some(value) => {
            let value = parse_canonical_u64(value).ok_or_else(|| {
                ProjectApiError::invalid("invalid_project_limit", request_id.to_owned())
            })?;
            let value = u32::try_from(value).map_err(|_| {
                ProjectApiError::invalid("invalid_project_limit", request_id.to_owned())
            })?;
            if value == 0 || value > MAX_PROJECT_LIST_LIMIT {
                return Err(ProjectApiError::invalid(
                    "invalid_project_limit",
                    request_id.to_owned(),
                ));
            }
            value
        }
    };
    Ok((filter, limit))
}

pub fn parse_decimal_version(value: &str) -> Result<i64, DecimalVersionError> {
    let parsed = parse_canonical_u64(value).ok_or(DecimalVersionError)?;
    i64::try_from(parsed)
        .ok()
        .filter(|parsed| *parsed >= 1)
        .ok_or(DecimalVersionError)
}

fn parse_canonical_u64(value: &str) -> Option<u64> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return None;
    }
    value.parse().ok()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecimalVersionError;

fn format_timestamp(value: OffsetDateTime) -> Result<String, ProjectApiError> {
    value
        .to_offset(UtcOffset::UTC)
        .format(&Rfc3339)
        .map_err(|_| ProjectApiError::unavailable(OperationId::new().to_string()))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorEnvelope {
    code: &'static str,
    message: &'static str,
    request_id: String,
    details: BTreeMap<String, String>,
}

pub struct ProjectApiError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    request_id: String,
}

impl ProjectApiError {
    fn invalid(code: &'static str, request_id: String) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code,
            message: "The project request is invalid.",
            request_id,
        }
    }

    fn service(error: ProjectServiceError, request_id: String) -> Self {
        let (status, message) = match error {
            ProjectServiceError::NotFound => (StatusCode::NOT_FOUND, "The project was not found."),
            ProjectServiceError::Stale => (
                StatusCode::CONFLICT,
                "The project was changed by another request.",
            ),
            ProjectServiceError::Conflict => (
                StatusCode::CONFLICT,
                "The project destination already exists.",
            ),
            ProjectServiceError::IdempotencyConflict => (
                StatusCode::CONFLICT,
                "The idempotency key was used for a different request.",
            ),
            ProjectServiceError::InProgress => (
                StatusCode::CONFLICT,
                "Project creation is still in progress or requires recovery.",
            ),
            ProjectServiceError::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "The project service is temporarily unavailable.",
            ),
        };
        Self {
            status,
            code: error.code(),
            message,
            request_id,
        }
    }

    fn unavailable(request_id: String) -> Self {
        Self::service(ProjectServiceError::Unavailable, request_id)
    }

    fn method_not_allowed(request_id: String) -> Self {
        Self {
            status: StatusCode::METHOD_NOT_ALLOWED,
            code: "method_not_allowed",
            message: "The request method is not allowed for this project resource.",
            request_id,
        }
    }
}

impl fmt::Debug for ProjectApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProjectApiError")
            .field("status", &self.status)
            .field("code", &self.code)
            .field("request_id", &"<redacted>")
            .finish()
    }
}

impl IntoResponse for ProjectApiError {
    fn into_response(self) -> Response {
        let body = ErrorEnvelope {
            code: self.code,
            message: self.message,
            request_id: self.request_id.clone(),
            details: BTreeMap::new(),
        };
        (self.status, [(REQUEST_ID, self.request_id)], Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_query_is_exact_and_bounded() {
        let request_id = "request";
        assert_eq!(
            parse_list_query(Some("status=all&limit=10"), request_id).unwrap(),
            (ProjectListFilter::All, 10)
        );
        for query in [
            "status=unknown",
            "limit=0",
            "limit=01",
            "limit=101",
            "limit=1&limit=2",
            "other=x",
        ] {
            assert!(parse_list_query(Some(query), request_id).is_err());
        }
    }
}
