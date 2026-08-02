use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::{Extension, RawQuery, Request};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
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
use cellar_storage::{RangeDecision, decide_range};
use futures_util::stream::try_unfold;
use serde::{Deserialize, Serialize};
use time::UtcOffset;
use time::format_description::well_known::Rfc3339;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::session::SessionState;

pub const MAX_FILE_CURSOR_BYTES: usize = 4 * 1024;
pub const MAX_FILE_QUERY_BYTES: usize = 8 * 1024;
pub const FILE_CURSOR_VERSION: u8 = 1;
pub const DOWNLOAD_CHUNK_BYTES: usize = 64 * 1024;
pub const MAX_OPEN_DOWNLOADS: usize = 8;

const REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");
const MAX_REQUEST_ID_BYTES: usize = 128;

#[derive(Clone)]
struct FilesState {
    service: FileService,
    downloads: Arc<dyn DownloadSource>,
    download_permits: Arc<Semaphore>,
}

pub fn files_router<S>(service: FileService) -> Router<SessionState<S>>
where
    S: EnrollmentStore + 'static,
{
    files_router_with_downloads(service, Arc::new(DisabledDownloadSource))
}

pub fn files_router_with_downloads<S>(
    service: FileService,
    downloads: Arc<dyn DownloadSource>,
) -> Router<SessionState<S>>
where
    S: EnrollmentStore + 'static,
{
    Router::new()
        .route(
            "/api/v1/projects/{project_id}/files",
            get(list_files).fallback(method_not_allowed),
        )
        .route(
            "/api/v1/projects/{project_id}/files/{file_id}/{action}",
            get(download_file).fallback(method_not_allowed),
        )
        .route(
            "/api/v1/projects/{project_id}/files/",
            any(invalid_file_path),
        )
        .route(
            "/api/v1/projects/{project_id}/files/{file_id}",
            any(invalid_file_path),
        )
        .route(
            "/api/v1/projects/{project_id}/files/{file_id}/{action}/",
            any(invalid_file_path),
        )
        .route(
            "/api/v1/projects/{project_id}/files/{file_id}/{action}/{*rest}",
            any(invalid_file_path),
        )
        .layer(Extension(FilesState {
            service,
            downloads,
            download_permits: Arc::new(Semaphore::new(MAX_OPEN_DOWNLOADS)),
        }))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DownloadMetadata {
    filename: String,
    length: u64,
    ready_sha256: Option<[u8; 32]>,
}

impl DownloadMetadata {
    pub fn new(
        filename: impl Into<String>,
        length: u64,
        ready_sha256: Option<[u8; 32]>,
    ) -> Result<Self, DownloadMetadataError> {
        let filename = filename.into();
        if filename.is_empty() || filename.len() > 4 * 1024 || length > i64::MAX as u64 {
            return Err(DownloadMetadataError);
        }
        Ok(Self {
            filename,
            length,
            ready_sha256,
        })
    }

    #[must_use]
    pub fn filename(&self) -> &str {
        &self.filename
    }

    #[must_use]
    pub const fn length(&self) -> u64 {
        self.length
    }

    #[must_use]
    pub const fn ready_sha256(&self) -> Option<[u8; 32]> {
        self.ready_sha256
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DownloadMetadataError;

impl fmt::Display for DownloadMetadataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid_download_metadata")
    }
}

impl std::error::Error for DownloadMetadataError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DownloadSpan {
    start: u64,
    length: u64,
}

impl DownloadSpan {
    pub fn new(start: u64, length: u64) -> Result<Self, DownloadMetadataError> {
        if start
            .checked_add(length)
            .is_none_or(|end| end > i64::MAX as u64)
        {
            return Err(DownloadMetadataError);
        }
        Ok(Self { start, length })
    }

    #[must_use]
    pub const fn start(self) -> u64 {
        self.start
    }

    #[must_use]
    pub const fn length(self) -> u64 {
        self.length
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DownloadError {
    ProjectNotFound,
    FileNotFound,
    NotAFile,
    Settling,
    Unsupported,
    IdentityChanged,
    Saturated,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DownloadReadError {
    Io,
    UnexpectedEof,
}

impl fmt::Display for DownloadReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Io => "download_read_failed",
            Self::UnexpectedEof => "download_short_read",
        })
    }
}

impl std::error::Error for DownloadReadError {}

/// An opened download whose metadata, verification, and body all refer to the
/// same stable filesystem handle.
#[async_trait]
pub trait VerifiedDownload: Send {
    fn metadata(&self) -> &DownloadMetadata;

    async fn verify(&self) -> Result<(), DownloadError>;

    /// Pulls exactly the requested bounded chunk from this same verified
    /// handle. Returning fewer or more bytes is treated as a stream failure.
    async fn read_exact_chunk(&mut self, span: DownloadSpan) -> Result<Vec<u8>, DownloadReadError>;
}

#[async_trait]
pub trait DownloadSource: Send + Sync {
    /// Opens and validates project/catalog state, then returns a stable handle.
    ///
    /// Implementations accept active and archived projects, reject deleted
    /// projects and non-live file states with the matching `DownloadError`,
    /// compare the catalog identity/size/mtime against the opened handle, and
    /// schedule reconciliation before returning `IdentityChanged`. A SHA-256
    /// is supplied in `DownloadMetadata` only when its catalog state is ready.
    async fn open_verified(
        &self,
        project_id: ProjectId,
        file_id: FileEntryId,
    ) -> Result<Box<dyn VerifiedDownload>, DownloadError>;
}

struct DisabledDownloadSource;

#[async_trait]
impl DownloadSource for DisabledDownloadSource {
    async fn open_verified(
        &self,
        _: ProjectId,
        _: FileEntryId,
    ) -> Result<Box<dyn VerifiedDownload>, DownloadError> {
        Err(DownloadError::Unavailable)
    }
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

async fn download_file(
    Extension(state): Extension<FilesState>,
    request: Request,
) -> Result<Response, FileApiError> {
    let request_id = request_id(request.headers())?;
    let (project_id, file_id) = parse_download_path(request.uri().path(), &request_id)?;
    let permit = if request.method() == Method::HEAD {
        None
    } else {
        Some(
            state
                .download_permits
                .clone()
                .try_acquire_owned()
                .map_err(|_| {
                    FileApiError::download(DownloadError::Saturated, request_id.clone())
                })?,
        )
    };
    let download = state
        .downloads
        .open_verified(project_id, file_id)
        .await
        .map_err(|error| FileApiError::download(error, request_id.clone()))?;
    let metadata = download.metadata().clone();
    download
        .verify()
        .await
        .map_err(|error| FileApiError::download(error, request_id.clone()))?;

    let etag = metadata.ready_sha256.map(strong_sha256_etag);
    let decision = match header_value(request.headers(), header::RANGE) {
        HeaderField::Value(range) => match header_value(request.headers(), header::IF_RANGE) {
            HeaderField::Missing => decide_range(Some(&range), metadata.length),
            HeaderField::Value(if_range) if etag.as_deref() == Some(if_range.as_str()) => {
                decide_range(Some(&range), metadata.length)
            }
            HeaderField::Value(_) | HeaderField::Invalid => RangeDecision::Full,
        },
        HeaderField::Missing | HeaderField::Invalid => RangeDecision::Full,
    };
    let (status, span, content_range) = match decision {
        RangeDecision::Full => (
            StatusCode::OK,
            DownloadSpan {
                start: 0,
                length: metadata.length,
            },
            None,
        ),
        RangeDecision::Partial {
            start,
            end_inclusive,
        } => (
            StatusCode::PARTIAL_CONTENT,
            DownloadSpan {
                start,
                length: end_inclusive - start + 1,
            },
            Some(format!("bytes {start}-{end_inclusive}/{}", metadata.length)),
        ),
        RangeDecision::Unsatisfiable { len } => {
            return Err(FileApiError::range(len, request_id));
        }
    };

    let mut headers = download_headers(&metadata, &request_id)?;
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&span.length.to_string())
            .map_err(|_| FileApiError::unavailable(request_id.clone()))?,
    );
    if let Some(content_range) = content_range {
        headers.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&content_range)
                .map_err(|_| FileApiError::unavailable(request_id.clone()))?,
        );
    }
    if let Some(etag) = etag {
        headers.insert(
            header::ETAG,
            HeaderValue::from_str(&etag)
                .map_err(|_| FileApiError::unavailable(request_id.clone()))?,
        );
    }
    let body = if request.method() == Method::HEAD {
        Body::empty()
    } else {
        download_body(
            download,
            span,
            permit.ok_or_else(|| FileApiError::unavailable(request_id.clone()))?,
        )
    };
    Ok((status, headers, body).into_response())
}

struct DownloadBodyState {
    download: Box<dyn VerifiedDownload>,
    offset: u64,
    remaining: u64,
    _permit: OwnedSemaphorePermit,
}

fn download_body(
    download: Box<dyn VerifiedDownload>,
    span: DownloadSpan,
    permit: OwnedSemaphorePermit,
) -> Body {
    let stream = try_unfold(
        DownloadBodyState {
            download,
            offset: span.start,
            remaining: span.length,
            _permit: permit,
        },
        |mut state| async move {
            if state.remaining == 0 {
                return Ok(None);
            }
            let length = state.remaining.min(DOWNLOAD_CHUNK_BYTES as u64);
            let span = DownloadSpan {
                start: state.offset,
                length,
            };
            let bytes = state.download.read_exact_chunk(span).await?;
            if bytes.len() != length as usize {
                return Err(DownloadReadError::UnexpectedEof);
            }
            state.offset = state
                .offset
                .checked_add(length)
                .ok_or(DownloadReadError::Io)?;
            state.remaining -= length;
            Ok(Some((bytes, state)))
        },
    );
    Body::from_stream(stream)
}

enum HeaderField {
    Missing,
    Value(String),
    Invalid,
}

fn header_value(headers: &HeaderMap, name: HeaderName) -> HeaderField {
    let values: Vec<_> = headers.get_all(name).iter().collect();
    match values.as_slice() {
        [] => HeaderField::Missing,
        [value] => value
            .to_str()
            .map(|value| HeaderField::Value(value.to_owned()))
            .unwrap_or(HeaderField::Invalid),
        _ => HeaderField::Invalid,
    }
}

fn strong_sha256_etag(hash: [u8; 32]) -> String {
    format!("\"sha256-{}\"", URL_SAFE_NO_PAD.encode(hash))
}

fn download_headers(
    metadata: &DownloadMetadata,
    request_id: &str,
) -> Result<HeaderMap, FileApiError> {
    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        REQUEST_ID,
        HeaderValue::from_str(request_id)
            .map_err(|_| FileApiError::unavailable(request_id.to_owned()))?,
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&content_disposition(&metadata.filename))
            .map_err(|_| FileApiError::unavailable(request_id.to_owned()))?,
    );
    Ok(headers)
}

fn content_disposition(filename: &str) -> String {
    let sanitized: String = filename
        .chars()
        .filter(|character| {
            !character.is_control() && !matches!(character, '/' | '\\' | '\r' | '\n')
        })
        .collect();
    let sanitized = if sanitized.is_empty() {
        "download"
    } else {
        &sanitized
    };
    let fallback: String = sanitized
        .chars()
        .map(|character| {
            if character.is_ascii_graphic() && !matches!(character, '"' | '\\') {
                character
            } else {
                '_'
            }
        })
        .collect();
    let encoded = percent_encode_filename(sanitized);
    format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}")
}

fn percent_encode_filename(filename: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(filename.len());
    for byte in filename.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            )
        {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[(byte >> 4) as usize]));
            encoded.push(char::from(HEX[(byte & 0x0f) as usize]));
        }
    }
    encoded
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

fn parse_download_path(
    path: &str,
    request_id: &str,
) -> Result<(ProjectId, FileEntryId), FileApiError> {
    let invalid = || FileApiError::invalid("invalid_file_path", request_id.to_owned());
    let tail = path
        .strip_prefix("/api/v1/projects/")
        .and_then(|tail| tail.strip_suffix("/download"))
        .ok_or_else(invalid)?;
    let (project, file) = tail.split_once("/files/").ok_or_else(invalid)?;
    if project.is_empty()
        || file.is_empty()
        || project.contains(['/', '\\', '%'])
        || file.contains(['/', '\\', '%'])
    {
        return Err(invalid());
    }
    let project_id: ProjectId = project.parse().map_err(|_| invalid())?;
    let file_id: FileEntryId = file.parse().map_err(|_| invalid())?;
    if project_id.to_string() != project || file_id.to_string() != file {
        return Err(invalid());
    }
    Ok((project_id, file_id))
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
    content_range: Option<String>,
    retry_after: bool,
}

impl FileApiError {
    fn invalid(code: &'static str, request_id: String) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code,
            message: "The file listing request is invalid.",
            request_id,
            content_range: None,
            retry_after: false,
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
            content_range: None,
            retry_after: false,
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
            content_range: None,
            retry_after: false,
        }
    }

    fn download(error: DownloadError, request_id: String) -> Self {
        let (status, code, message) = match error {
            DownloadError::ProjectNotFound => (
                StatusCode::NOT_FOUND,
                "project_not_found",
                "The project was not found.",
            ),
            DownloadError::FileNotFound => (
                StatusCode::NOT_FOUND,
                "file_not_found",
                "The file was not found.",
            ),
            DownloadError::NotAFile => (
                StatusCode::CONFLICT,
                "not_a_file",
                "The requested entry is not a downloadable file.",
            ),
            DownloadError::Settling => (
                StatusCode::CONFLICT,
                "file_settling",
                "The file is still settling and cannot be downloaded yet.",
            ),
            DownloadError::Unsupported => (
                StatusCode::CONFLICT,
                "unsupported_file_entry",
                "The file cannot be accessed safely.",
            ),
            DownloadError::IdentityChanged => (
                StatusCode::CONFLICT,
                "file_identity_changed",
                "The file changed before the download could start.",
            ),
            DownloadError::Saturated => (
                StatusCode::TOO_MANY_REQUESTS,
                "download_capacity_exhausted",
                "Too many downloads are already open.",
            ),
            DownloadError::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "file_storage_unavailable",
                "File storage is temporarily unavailable.",
            ),
        };
        Self {
            status,
            code,
            message,
            request_id,
            content_range: None,
            retry_after: error == DownloadError::Saturated,
        }
    }

    fn range(len: u64, request_id: String) -> Self {
        Self {
            status: StatusCode::RANGE_NOT_SATISFIABLE,
            code: "invalid_range",
            message: "The requested byte range cannot be satisfied.",
            request_id,
            content_range: Some(format!("bytes */{len}")),
            retry_after: false,
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
        let mut response =
            (self.status, [(REQUEST_ID, self.request_id)], Json(body)).into_response();
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-store"),
        );
        if self.retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        }
        response.headers_mut().insert(
            HeaderName::from_static("x-content-type-options"),
            HeaderValue::from_static("nosniff"),
        );
        response.headers_mut().insert(
            HeaderName::from_static("cross-origin-resource-policy"),
            HeaderValue::from_static("same-origin"),
        );
        if let Some(content_range) = self.content_range
            && let Ok(value) = HeaderValue::from_str(&content_range)
        {
            response.headers_mut().insert(header::CONTENT_RANGE, value);
            response
                .headers_mut()
                .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
        }
        response
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
        assert!(DownloadSpan::new(i64::MAX as u64, 1).is_err());
    }
}
