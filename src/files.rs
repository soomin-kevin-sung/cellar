//! Project file listing and bounded streaming downloads.

use std::{future::Future, pin::Pin, sync::Arc, time::SystemTime};

use axum::{
    Extension, Json, Router,
    body::Body,
    extract::{Path, State, rejection::PathRejection},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Serialize;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::{
    app::RequestId,
    auth::OwnerIdentity,
    db::{Database, DbError, ProjectRow},
    error::AppError,
    storage::{DiskFile, SafeFileName, Storage, StorageError},
    uploads::DecimalU64,
};

type RepositoryFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, DbError>> + Send + 'a>>;
type StorageFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, StorageError>> + Send + 'a>>;

/// Database operations needed to establish that a project is committed.
pub trait FileRepository: Send + Sync {
    fn get_project<'a>(&'a self, project_id: Uuid) -> RepositoryFuture<'a, Option<ProjectRow>>;
}

impl FileRepository for Database {
    fn get_project<'a>(&'a self, project_id: Uuid) -> RepositoryFuture<'a, Option<ProjectRow>> {
        Box::pin(async move { self.get_project(project_id).await })
    }
}

/// Storage operations needed by file reads.
pub trait FileStorage: Send + Sync {
    fn list_files(&self, project_id: Uuid) -> StorageFuture<'_, Vec<DiskFile>>;
    fn open_file<'a>(&'a self, project_id: Uuid, name: &'a SafeFileName)
    -> StorageFuture<'a, File>;
}

impl FileStorage for Storage {
    fn list_files(&self, project_id: Uuid) -> StorageFuture<'_, Vec<DiskFile>> {
        Box::pin(async move { self.list_files(project_id).await })
    }

    fn open_file<'a>(
        &'a self,
        project_id: Uuid,
        name: &'a SafeFileName,
    ) -> StorageFuture<'a, File> {
        Box::pin(async move { self.open_final_file(project_id, name).await })
    }
}

#[derive(Clone)]
pub struct FileService {
    repository: Arc<dyn FileRepository>,
    storage: Arc<dyn FileStorage>,
}

impl FileService {
    pub fn new(repository: Arc<dyn FileRepository>, storage: Arc<dyn FileStorage>) -> Self {
        Self {
            repository,
            storage,
        }
    }

    async fn require_project(
        &self,
        project_id: Uuid,
        request_id: &RequestId,
    ) -> Result<(), AppError> {
        match self.repository.get_project(project_id).await {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(project_not_found(request_id.clone())),
            Err(_) => {
                record_failure("database_unavailable", request_id, project_id);
                Err(files_unavailable(request_id.clone()))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ByteRange {
    pub start: u64,
    pub end_inclusive: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidByteRange;

impl ByteRange {
    #[must_use]
    pub const fn new(start: u64, end_inclusive: u64) -> Self {
        Self {
            start,
            end_inclusive,
        }
    }

    /// Parses one strict RFC 9110 `bytes` range against a known representation size.
    pub fn parse(value: &str, size: u64) -> Result<Self, InvalidByteRange> {
        if size == 0 {
            return Err(InvalidByteRange);
        }
        let bytes = value.as_bytes();
        if bytes.len() < 6 || !bytes[..6].eq_ignore_ascii_case(b"bytes=") {
            return Err(InvalidByteRange);
        }
        let range = &value[6..];
        if range.is_empty()
            || range.contains(',')
            || range.bytes().any(|byte| byte.is_ascii_whitespace())
        {
            return Err(InvalidByteRange);
        }
        let (start, end) = range.split_once('-').ok_or(InvalidByteRange)?;
        if start.is_empty() {
            let suffix = parse_u64(end)?;
            if suffix == 0 {
                return Err(InvalidByteRange);
            }
            let start = size.saturating_sub(suffix);
            return Ok(Self::new(start, size - 1));
        }

        let start = parse_u64(start)?;
        if start >= size {
            return Err(InvalidByteRange);
        }
        let end_inclusive = if end.is_empty() {
            size - 1
        } else {
            let end = parse_u64(end)?;
            if start > end {
                return Err(InvalidByteRange);
            }
            end.min(size - 1)
        };
        Ok(Self::new(start, end_inclusive))
    }

    const fn len(self) -> u64 {
        self.end_inclusive - self.start + 1
    }
}

fn parse_u64(value: &str) -> Result<u64, InvalidByteRange> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(InvalidByteRange);
    }
    value.parse().map_err(|_| InvalidByteRange)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FileResponse {
    name: String,
    size: DecimalU64,
    modified_at: String,
}

pub fn file_router(service: FileService) -> Router {
    Router::new()
        .route("/api/v1/projects/{project_id}/files", get(list_files))
        .route(
            "/api/v1/projects/{project_id}/files/{file_name}",
            get(download_file).head(download_file),
        )
        .with_state(service)
}

async fn list_files(
    State(service): State<FileService>,
    Extension(request_id): Extension<RequestId>,
    Extension(_owner): Extension<OwnerIdentity>,
    path: Result<Path<String>, PathRejection>,
) -> Result<Json<Vec<FileResponse>>, AppError> {
    let Path(project_id) = path.map_err(|_| invalid_request(request_id.clone()))?;
    let project_id =
        parse_canonical_uuid(&project_id).ok_or_else(|| invalid_request(request_id.clone()))?;
    service.require_project(project_id, &request_id).await?;
    let files = service.storage.list_files(project_id).await.map_err(|_| {
        record_failure("storage_unavailable", &request_id, project_id);
        files_unavailable(request_id.clone())
    })?;
    let mut files = files
        .into_iter()
        .map(file_response)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|InvalidModifiedTime| {
            record_failure("invalid_modified_time", &request_id, project_id);
            files_unavailable(request_id.clone())
        })?;
    files.sort_by(|left, right| compare_names(&left.name, &right.name));
    Ok(Json(files))
}

#[derive(Clone, Copy, Debug)]
struct InvalidModifiedTime;

fn file_response(file: DiskFile) -> Result<FileResponse, InvalidModifiedTime> {
    let modified_at = format_modified_at(file.modified_at())?;
    Ok(FileResponse {
        name: file.name().as_str().to_owned(),
        size: DecimalU64(file.size()),
        modified_at,
    })
}

fn format_modified_at(value: SystemTime) -> Result<String, InvalidModifiedTime> {
    let timestamp = match value.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => {
            let duration = time::Duration::try_from(duration).map_err(|_| InvalidModifiedTime)?;
            OffsetDateTime::UNIX_EPOCH
                .checked_add(duration)
                .ok_or(InvalidModifiedTime)?
        }
        Err(error) => {
            let duration =
                time::Duration::try_from(error.duration()).map_err(|_| InvalidModifiedTime)?;
            OffsetDateTime::UNIX_EPOCH
                .checked_sub(duration)
                .ok_or(InvalidModifiedTime)?
        }
    };
    timestamp.format(&Rfc3339).map_err(|_| InvalidModifiedTime)
}

fn compare_names(left: &str, right: &str) -> std::cmp::Ordering {
    left.to_lowercase()
        .cmp(&right.to_lowercase())
        .then_with(|| left.cmp(right))
}

async fn download_file(
    State(service): State<FileService>,
    Extension(request_id): Extension<RequestId>,
    Extension(_owner): Extension<OwnerIdentity>,
    method: Method,
    headers: HeaderMap,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Result<Response, AppError> {
    let Path((project_id, file_name)) = path.map_err(|_| invalid_file_name(request_id.clone()))?;
    let project_id =
        parse_canonical_uuid(&project_id).ok_or_else(|| invalid_request(request_id.clone()))?;
    let file_name =
        SafeFileName::parse(file_name).map_err(|_| invalid_file_name(request_id.clone()))?;
    service.require_project(project_id, &request_id).await?;

    let mut file = match service.storage.open_file(project_id, &file_name).await {
        Ok(file) => file,
        Err(StorageError::NotFound | StorageError::UnsafeEntry) => {
            return Err(file_not_found(request_id));
        }
        Err(_) => {
            record_failure("storage_unavailable", &request_id, project_id);
            return Err(files_unavailable(request_id));
        }
    };
    let metadata = file.metadata().await.map_err(|_| {
        record_failure("storage_unavailable", &request_id, project_id);
        files_unavailable(request_id.clone())
    })?;
    if !metadata.is_file() {
        return Err(file_not_found(request_id));
    }
    let size = metadata.len();
    let selected = requested_range(&headers, size, &request_id)?;
    let (status, start, content_len) = selected.map_or((StatusCode::OK, 0, size), |range| {
        (StatusCode::PARTIAL_CONTENT, range.start, range.len())
    });

    let mut response_headers = HeaderMap::new();
    response_headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    response_headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&content_len.to_string()).expect("u64 is a valid header value"),
    );
    response_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(
            mime_guess::from_path(file_name.as_str())
                .first_or_octet_stream()
                .as_ref(),
        )
        .expect("MIME types are valid header values"),
    );
    response_headers.insert(header::CONTENT_DISPOSITION, content_disposition(&file_name));
    if let Some(range) = selected {
        response_headers.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!(
                "bytes {}-{}/{}",
                range.start, range.end_inclusive, size
            ))
            .expect("u64 byte positions are valid header values"),
        );
    }

    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        if start != 0 {
            file.seek(std::io::SeekFrom::Start(start))
                .await
                .map_err(|_| {
                    record_failure("storage_seek_failed", &request_id, project_id);
                    files_unavailable(request_id.clone())
                })?;
        }
        Body::from_stream(ReaderStream::new(file.take(content_len)))
    };
    Ok((status, response_headers, body).into_response())
}

fn requested_range(
    headers: &HeaderMap,
    size: u64,
    request_id: &RequestId,
) -> Result<Option<ByteRange>, AppError> {
    let mut values = headers.get_all(header::RANGE).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(AppError::range_not_satisfiable(request_id.clone(), size));
    }
    let value = value
        .to_str()
        .map_err(|_| AppError::range_not_satisfiable(request_id.clone(), size))?;
    ByteRange::parse(value, size)
        .map(Some)
        .map_err(|InvalidByteRange| AppError::range_not_satisfiable(request_id.clone(), size))
}

fn content_disposition(name: &SafeFileName) -> HeaderValue {
    let fallback = name
        .as_str()
        .chars()
        .map(|character| {
            if character.is_ascii() && !character.is_ascii_control() {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let encoded = name
        .as_str()
        .as_bytes()
        .iter()
        .fold(String::new(), |mut encoded, &byte| {
            if byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'&'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
            {
                encoded.push(char::from(byte));
            } else {
                use std::fmt::Write;
                write!(&mut encoded, "%{byte:02X}").expect("writing to String cannot fail");
            }
            encoded
        });
    HeaderValue::from_str(&format!(
        "attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}"
    ))
    .expect("validated file names produce safe disposition headers")
}

fn parse_canonical_uuid(value: &str) -> Option<Uuid> {
    let id = Uuid::parse_str(value).ok()?;
    (id.hyphenated().to_string() == value).then_some(id)
}

fn invalid_request(request_id: RequestId) -> AppError {
    AppError::bad_request(request_id, "invalid_request", "The request is invalid.")
}

fn invalid_file_name(request_id: RequestId) -> AppError {
    AppError::bad_request(request_id, "invalid_file_name", "The file name is invalid.")
}

fn project_not_found(request_id: RequestId) -> AppError {
    AppError::not_found(
        request_id,
        "project_not_found",
        "The project was not found.",
    )
}

fn file_not_found(request_id: RequestId) -> AppError {
    AppError::not_found(request_id, "file_not_found", "The file was not found.")
}

fn files_unavailable(request_id: RequestId) -> AppError {
    AppError::service_unavailable(
        request_id,
        "files_unavailable",
        "Files are temporarily unavailable.",
    )
}

fn record_failure(reason: &'static str, request_id: &RequestId, project_id: Uuid) {
    tracing::warn!(
        reason,
        request_id = %request_id,
        project_id = %project_id,
        "file operation failed"
    );
}

#[cfg(test)]
mod tests {
    use std::{cmp::Ordering, time::SystemTime};

    use super::{compare_names, format_modified_at};

    #[test]
    fn names_sort_case_insensitively_with_exact_tie_breaker() {
        assert_eq!(compare_names("alpha", "Beta"), Ordering::Less);
        assert_eq!(compare_names("A.txt", "a.TXT"), Ordering::Less);
        assert_eq!(compare_names("same", "same"), Ordering::Equal);
    }

    #[test]
    fn modified_time_formatter_is_canonical_utc() {
        assert_eq!(
            format_modified_at(SystemTime::UNIX_EPOCH).unwrap(),
            "1970-01-01T00:00:00Z"
        );
        if let Some(out_of_range) = SystemTime::UNIX_EPOCH.checked_add(std::time::Duration::MAX) {
            assert!(format_modified_at(out_of_range).is_err());
        }
    }
}
