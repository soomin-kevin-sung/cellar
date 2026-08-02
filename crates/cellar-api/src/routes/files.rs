use std::collections::BTreeMap;
use std::fmt;

use axum::extract::{Extension, RawQuery, Request};
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cellar_auth::EnrollmentStore;
use cellar_core::{
    DEFAULT_FILE_LIST_LIMIT, FileCursor, FileEntry, FileEntryId, FileListRequest,
    FileRepositoryError, FileService, MAX_FILE_LIST_LIMIT, OperationId, ProjectId,
};
use serde::{Deserialize, Serialize};
use time::UtcOffset;
use time::format_description::well_known::Rfc3339;

use super::session::SessionState;

pub const MAX_FILE_CURSOR_BYTES: usize = 4 * 1024;
pub const MAX_FILE_QUERY_BYTES: usize = 8 * 1024;
pub const FILE_CURSOR_VERSION: u8 = 1;

const REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");
const MAX_REQUEST_ID_BYTES: usize = 128;

#[derive(Clone)]
struct FilesState {
    service: FileService,
}

pub fn files_router<S>(service: FileService) -> Router<SessionState<S>>
where
    S: EnrollmentStore + 'static,
{
    Router::new()
        .route(
            "/api/v1/projects/{project_id}/files",
            get(list_files).fallback(method_not_allowed),
        )
        .route(
            "/api/v1/projects/{project_id}/files/",
            any(invalid_file_path),
        )
        .route(
            "/api/v1/projects/{project_id}/files/{*rest}",
            any(invalid_file_path),
        )
        .layer(Extension(FilesState { service }))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FilePageBody {
    items: Vec<FileEntryBody>,
    next_cursor: Option<String>,
    snapshot_version: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FileEntryBody {
    id: String,
    project_id: String,
    parent_id: Option<String>,
    exact_name: String,
    relative_path: String,
    kind: cellar_core::FileKind,
    size: String,
    mtime_filetime_100ns: String,
    hash: Option<String>,
    hash_state: cellar_core::FileHashState,
    state: cellar_core::FileState,
    revision: String,
    scan_generation: String,
    observed_at: String,
}

fn file_entry_body(entry: FileEntry, request_id: &str) -> Result<FileEntryBody, FileApiError> {
    Ok(FileEntryBody {
        id: entry.id.to_string(),
        project_id: entry.project_id.to_string(),
        parent_id: entry.parent_id.map(|id| id.to_string()),
        exact_name: entry.exact_name.as_str().to_owned(),
        relative_path: entry.relative_path,
        kind: entry.kind,
        size: entry.size.to_string(),
        mtime_filetime_100ns: entry.mtime_filetime_100ns.to_string(),
        hash: entry.hash.map(|hash| URL_SAFE_NO_PAD.encode(hash)),
        hash_state: entry.hash_state,
        state: entry.state,
        revision: entry.revision.to_string(),
        scan_generation: entry.scan_generation.to_string(),
        observed_at: entry
            .observed_at
            .to_offset(UtcOffset::UTC)
            .format(&Rfc3339)
            .map_err(|_| FileApiError::unavailable(request_id.to_owned()))?,
    })
}

async fn list_files(
    Extension(state): Extension<FilesState>,
    RawQuery(query): RawQuery,
    request: Request,
) -> Result<impl IntoResponse, FileApiError> {
    let request_id = request_id(request.headers())?;
    let project_id = parse_project_file_path(request.uri().path(), &request_id)?;
    let parsed = parse_list_query(query.as_deref(), project_id, &request_id)?;
    let page = state
        .service
        .list(parsed)
        .await
        .map_err(|error| FileApiError::repository(error, request_id.clone()))?;
    let next_cursor = page
        .next_cursor
        .as_ref()
        .map(encode_cursor)
        .transpose()
        .map_err(|_| FileApiError::unavailable(request_id.clone()))?;
    let items = page
        .items
        .into_iter()
        .map(|entry| file_entry_body(entry, &request_id))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((
        [(REQUEST_ID, request_id)],
        Json(FilePageBody {
            items,
            next_cursor,
            snapshot_version: page.snapshot_version.to_string(),
        }),
    ))
}

fn parse_list_query(
    query: Option<&str>,
    project_id: ProjectId,
    request_id: &str,
) -> Result<FileListRequest, FileApiError> {
    let query = query.unwrap_or_default();
    if query.len() > MAX_FILE_QUERY_BYTES {
        return Err(FileApiError::invalid(
            "invalid_file_query",
            request_id.to_owned(),
        ));
    }
    let mut parent_id = None;
    let mut parent_seen = false;
    let mut limit = None;
    let mut cursor = None;
    if !query.is_empty() {
        for field in query.split('&') {
            let (key, value) = field.split_once('=').ok_or_else(|| {
                FileApiError::invalid("invalid_file_query", request_id.to_owned())
            })?;
            match key {
                "parentId" if !parent_seen => {
                    parent_seen = true;
                    parent_id = Some(value.parse().map_err(|_| {
                        FileApiError::invalid("invalid_parent_id", request_id.to_owned())
                    })?);
                }
                "limit" if limit.is_none() => {
                    limit = Some(parse_limit(value, request_id)?);
                }
                "cursor" if cursor.is_none() => {
                    cursor = Some(decode_cursor(value, request_id)?);
                }
                _ => {
                    return Err(FileApiError::invalid(
                        "invalid_file_query",
                        request_id.to_owned(),
                    ));
                }
            }
        }
    }
    let limit = limit.unwrap_or(DEFAULT_FILE_LIST_LIMIT);
    match cursor {
        Some(cursor) if cursor.project_id() == project_id && cursor.parent_id() == parent_id => {
            FileListRequest::after(cursor, limit)
                .map_err(|_| FileApiError::invalid("invalid_file_limit", request_id.to_owned()))
        }
        Some(_) => Err(FileApiError::invalid(
            "invalid_file_cursor",
            request_id.to_owned(),
        )),
        None => FileListRequest::first(project_id, parent_id, limit)
            .map_err(|_| FileApiError::invalid("invalid_file_limit", request_id.to_owned())),
    }
}

fn parse_limit(value: &str, request_id: &str) -> Result<u32, FileApiError> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return Err(FileApiError::invalid(
            "invalid_file_limit",
            request_id.to_owned(),
        ));
    }
    let limit = value
        .parse::<u32>()
        .map_err(|_| FileApiError::invalid("invalid_file_limit", request_id.to_owned()))?;
    if limit == 0 || limit > MAX_FILE_LIST_LIMIT {
        return Err(FileApiError::invalid(
            "invalid_file_limit",
            request_id.to_owned(),
        ));
    }
    Ok(limit)
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CursorEnvelope {
    version: u8,
    project_id: String,
    parent_id: Option<String>,
    snapshot_version: String,
    last_exact_name: String,
    last_entry_id: String,
}

fn encode_cursor(cursor: &FileCursor) -> Result<String, CursorCodecError> {
    let envelope = CursorEnvelope {
        version: FILE_CURSOR_VERSION,
        project_id: cursor.project_id().to_string(),
        parent_id: cursor.parent_id().map(|id| id.to_string()),
        snapshot_version: cursor.snapshot_version().to_string(),
        last_exact_name: cursor.exact_name().to_owned(),
        last_entry_id: cursor.entry_id().to_string(),
    };
    let encoded =
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&envelope).map_err(|_| CursorCodecError)?);
    if encoded.len() > MAX_FILE_CURSOR_BYTES {
        return Err(CursorCodecError);
    }
    Ok(encoded)
}

fn decode_cursor(value: &str, request_id: &str) -> Result<FileCursor, FileApiError> {
    let invalid = || FileApiError::invalid("invalid_file_cursor", request_id.to_owned());
    if value.is_empty()
        || value.len() > MAX_FILE_CURSOR_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(invalid());
    }
    let decoded = URL_SAFE_NO_PAD.decode(value).map_err(|_| invalid())?;
    let envelope: CursorEnvelope = serde_json::from_slice(&decoded).map_err(|_| invalid())?;
    let canonical = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&envelope).map_err(|_| invalid())?);
    if canonical != value || envelope.version != FILE_CURSOR_VERSION {
        return Err(invalid());
    }
    let project_id: ProjectId = envelope.project_id.parse().map_err(|_| invalid())?;
    if project_id.to_string() != envelope.project_id {
        return Err(invalid());
    }
    let parent_id = envelope
        .parent_id
        .map(|value| {
            let parsed: FileEntryId = value.parse().map_err(|_| invalid())?;
            if parsed.to_string() != value {
                return Err(invalid());
            }
            Ok(parsed)
        })
        .transpose()?;
    let entry_id: FileEntryId = envelope.last_entry_id.parse().map_err(|_| invalid())?;
    if entry_id.to_string() != envelope.last_entry_id {
        return Err(invalid());
    }
    let snapshot_version = parse_snapshot(&envelope.snapshot_version).ok_or_else(invalid)?;
    FileCursor::try_new(
        project_id,
        parent_id,
        snapshot_version,
        envelope.last_exact_name,
        entry_id,
    )
    .map_err(|_| invalid())
}

fn parse_snapshot(value: &str) -> Option<i64> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return None;
    }
    value.parse().ok()
}

fn parse_project_file_path(path: &str, request_id: &str) -> Result<ProjectId, FileApiError> {
    let project = path
        .strip_prefix("/api/v1/projects/")
        .and_then(|tail| tail.strip_suffix("/files"))
        .filter(|value| !value.is_empty() && !value.contains(['/', '\\', '%']))
        .ok_or_else(|| FileApiError::invalid("invalid_project_id", request_id.to_owned()))?;
    project
        .parse()
        .map_err(|_| FileApiError::invalid("invalid_project_id", request_id.to_owned()))
}

async fn invalid_file_path(headers: HeaderMap) -> FileApiError {
    match request_id(&headers) {
        Ok(request_id) => FileApiError::invalid("invalid_file_path", request_id),
        Err(error) => error,
    }
}

async fn method_not_allowed(headers: HeaderMap) -> FileApiError {
    match request_id(&headers) {
        Ok(request_id) => FileApiError::method_not_allowed(request_id),
        Err(error) => error,
    }
}

fn request_id(headers: &HeaderMap) -> Result<String, FileApiError> {
    let values: Vec<_> = headers.get_all(&REQUEST_ID).iter().collect();
    if values.is_empty() {
        return Ok(OperationId::new().to_string());
    }
    if values.len() != 1 {
        return Err(FileApiError::invalid(
            "invalid_request_id",
            OperationId::new().to_string(),
        ));
    }
    let value = values[0]
        .to_str()
        .map_err(|_| FileApiError::invalid("invalid_request_id", OperationId::new().to_string()))?;
    if value.is_empty()
        || value.len() > MAX_REQUEST_ID_BYTES
        || !value.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(FileApiError::invalid(
            "invalid_request_id",
            OperationId::new().to_string(),
        ));
    }
    Ok(value.to_owned())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CursorCodecError;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorEnvelope {
    code: &'static str,
    message: &'static str,
    request_id: String,
    details: BTreeMap<String, String>,
}

pub struct FileApiError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    request_id: String,
}

impl FileApiError {
    fn invalid(code: &'static str, request_id: String) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code,
            message: "The file listing request is invalid.",
            request_id,
        }
    }

    fn repository(error: FileRepositoryError, request_id: String) -> Self {
        let (status, message) = match error {
            FileRepositoryError::ProjectNotFound => {
                (StatusCode::NOT_FOUND, "The project was not found.")
            }
            FileRepositoryError::FolderNotFound => {
                (StatusCode::NOT_FOUND, "The folder was not found.")
            }
            FileRepositoryError::InvalidFolder => {
                (StatusCode::CONFLICT, "The requested entry is not a folder.")
            }
            FileRepositoryError::UnsupportedFolder => (
                StatusCode::CONFLICT,
                "The folder cannot be traversed safely.",
            ),
            FileRepositoryError::SnapshotChanged => (
                StatusCode::CONFLICT,
                "The file catalog changed; restart the listing.",
            ),
            FileRepositoryError::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "The file catalog is temporarily unavailable.",
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
        Self::repository(FileRepositoryError::Unavailable, request_id)
    }

    fn method_not_allowed(request_id: String) -> Self {
        Self {
            status: StatusCode::METHOD_NOT_ALLOWED,
            code: "method_not_allowed",
            message: "The request method is not allowed for this file resource.",
            request_id,
        }
    }
}

impl fmt::Debug for FileApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileApiError")
            .field("status", &self.status)
            .field("code", &self.code)
            .field("request_id", &"<redacted>")
            .finish()
    }
}

impl IntoResponse for FileApiError {
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
    fn list_query_and_cursor_codec_are_canonical_and_bounded() {
        let project = ProjectId::new();
        let entry = cellar_core::FileEntryId::new();
        let cursor = FileCursor::try_new(project, None, 3, "name.txt", entry).unwrap();
        let encoded = encode_cursor(&cursor).unwrap();
        assert_eq!(decode_cursor(&encoded, "request").unwrap(), cursor);
        for invalid in ["", "0", "01", "501", "+1", " 1"] {
            assert!(parse_limit(invalid, "request").is_err());
        }
        assert!(decode_cursor(&(encoded + "="), "request").is_err());
    }
}
