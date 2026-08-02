use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Extension, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cellar_auth::{
    AccessClaims, ClaimRequest, CsrfError, CsrfManager, EnrollmentError, EnrollmentMode,
    EnrollmentService, EnrollmentStore, MutationHeaders, RouteAccess, route_access,
};
use cellar_core::OperationId;
use serde::{Deserialize, Serialize};

const CSRF_HEADER: HeaderName = HeaderName::from_static("x-cellar-csrf");
const SEC_FETCH_SITE: HeaderName = HeaderName::from_static("sec-fetch-site");
const REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");
const MAX_CLAIM_BODY_BYTES: usize = 4 * 1024;
const MAX_REQUEST_ID_BYTES: usize = 128;

#[derive(Clone)]
pub struct RequestId(String);

#[must_use]
pub const fn claim_status(error: &EnrollmentError) -> StatusCode {
    match error {
        EnrollmentError::InvalidIdentity => StatusCode::UNAUTHORIZED,
        EnrollmentError::Forbidden => StatusCode::FORBIDDEN,
        EnrollmentError::NotFound => StatusCode::NOT_FOUND,
        EnrollmentError::Conflict => StatusCode::CONFLICT,
        EnrollmentError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
    }
}

#[must_use]
pub const fn csrf_status(error: &CsrfError) -> StatusCode {
    match error {
        CsrfError::Unauthenticated => StatusCode::UNAUTHORIZED,
        CsrfError::Forbidden => StatusCode::FORBIDDEN,
        CsrfError::Capacity => StatusCode::SERVICE_UNAVAILABLE,
        CsrfError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
    }
}

#[derive(Serialize)]
pub struct SessionResponse {
    pub csrf_token: String,
}

impl fmt::Debug for SessionResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionResponse")
            .field("csrf_token", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum SessionRouteError {
    NotFound,
    Csrf(CsrfError),
}

impl SessionRouteError {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotFound => "session_not_found",
            Self::Csrf(error) => error.code(),
        }
    }

    #[must_use]
    pub const fn status(&self) -> StatusCode {
        match self {
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Csrf(error) => csrf_status(error),
        }
    }
}

impl fmt::Debug for SessionRouteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

pub fn get_session(
    method: &str,
    manager: &CsrfManager,
    claims: &AccessClaims,
    owner_subject: &str,
    now_unix_seconds: i64,
) -> Result<SessionResponse, SessionRouteError> {
    if method != "GET" {
        return Err(SessionRouteError::NotFound);
    }
    let csrf_token = manager
        .issue(claims, owner_subject, now_unix_seconds)
        .map_err(SessionRouteError::Csrf)?;
    Ok(SessionResponse { csrf_token })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContentTypeError;

pub fn require_json_content_type<'a>(
    values: impl IntoIterator<Item = &'a [u8]>,
) -> Result<(), ContentTypeError> {
    let mut values = values.into_iter();
    let value = values.next().ok_or(ContentTypeError)?;
    if values.next().is_some() {
        return Err(ContentTypeError);
    }
    let value = std::str::from_utf8(value).map_err(|_| ContentTypeError)?;
    let media_type = value.split(';').next().unwrap_or_default().trim();
    if media_type == "application/json" {
        Ok(())
    } else {
        Err(ContentTypeError)
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum ClaimRouteError {
    InvalidRequest,
    UnsupportedMediaType,
    Forbidden,
    Enrollment(EnrollmentError),
}

impl ClaimRouteError {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_claim_request",
            Self::UnsupportedMediaType => "unsupported_media_type",
            Self::Forbidden => "claim_forbidden",
            Self::Enrollment(error) => error.code(),
        }
    }

    #[must_use]
    pub const fn status(&self) -> StatusCode {
        match self {
            Self::InvalidRequest => StatusCode::BAD_REQUEST,
            Self::UnsupportedMediaType => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::Enrollment(error) => claim_status(error),
        }
    }
}

impl fmt::Debug for ClaimRouteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

pub fn claim_owner<S: EnrollmentStore>(
    service: &EnrollmentService<S>,
    claims: &AccessClaims,
    content_types: &[&[u8]],
    origins: &[&[u8]],
    email: &str,
    code: &[u8; 32],
    now_unix_seconds: i64,
) -> Result<EnrollmentMode, ClaimRouteError> {
    require_json_content_type(content_types.iter().copied())
        .map_err(|_| ClaimRouteError::UnsupportedMediaType)?;
    if origins.len() != 1 {
        return Err(ClaimRouteError::Forbidden);
    }
    let origin = std::str::from_utf8(origins[0]).map_err(|_| ClaimRouteError::Forbidden)?;
    service
        .claim(
            claims,
            ClaimRequest {
                email,
                origin,
                code,
            },
            now_unix_seconds,
        )
        .map_err(ClaimRouteError::Enrollment)
}

type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub struct SessionState<S: EnrollmentStore> {
    enrollment: EnrollmentService<S>,
    csrf: Arc<CsrfManager>,
    clock: Clock,
}

impl<S: EnrollmentStore> Clone for SessionState<S> {
    fn clone(&self) -> Self {
        Self {
            enrollment: self.enrollment.clone(),
            csrf: Arc::clone(&self.csrf),
            clock: Arc::clone(&self.clock),
        }
    }
}

pub fn session_router<S, F>(
    enrollment: EnrollmentService<S>,
    csrf: Arc<CsrfManager>,
    clock: F,
) -> Router
where
    S: EnrollmentStore + 'static,
    F: Fn() -> i64 + Send + Sync + 'static,
{
    session_router_with_routes(enrollment, csrf, clock, Router::new())
}

pub fn session_router_with_routes<S, F>(
    enrollment: EnrollmentService<S>,
    csrf: Arc<CsrfManager>,
    clock: F,
    protected_routes: Router<SessionState<S>>,
) -> Router
where
    S: EnrollmentStore + 'static,
    F: Fn() -> i64 + Send + Sync + 'static,
{
    let state = SessionState {
        enrollment,
        csrf,
        clock: Arc::new(clock),
    };
    Router::new()
        .route("/owner/claim", post(claim_handler::<S>))
        .route("/api/v1/session", get(session_handler::<S>))
        .merge(protected_routes)
        .fallback(not_found)
        .layer(from_fn_with_state(state.clone(), security_boundary::<S>))
        .with_state(state)
}

async fn security_boundary<S: EnrollmentStore + 'static>(
    State(state): State<SessionState<S>>,
    mut request: Request,
    next: Next,
) -> Response {
    let (request_id, request_id_header) = match prepare_request_id(&mut request) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let request_id = request_id.0;
    let Some(claims) = request.extensions().get::<AccessClaims>().cloned() else {
        return ApiError::new(
            StatusCode::UNAUTHORIZED,
            "missing_authentication",
            "Authentication is required.",
            request_id,
        )
        .into_response();
    };
    let path = request.uri().path();
    let mode = match state.enrollment.mode() {
        Ok(mode) => mode,
        Err(error) => return ApiError::from_enrollment(error, request_id).into_response(),
    };
    let access = route_access(mode, path);
    let authorization = match state.enrollment.authorize(access, &claims) {
        Ok(authorization) => authorization,
        Err(error) => return ApiError::from_enrollment(error, request_id).into_response(),
    };
    if path != "/owner/claim" {
        let owner_subject = match authorization.owner_subject() {
            Some(subject) => subject,
            None => {
                return ApiError::from_enrollment(EnrollmentError::Forbidden, request_id)
                    .into_response();
            }
        };
        let headers = request.headers();
        let mutation_headers = MutationHeaders {
            origins: header_values(headers, &header::ORIGIN),
            csrf_tokens: header_values(headers, &CSRF_HEADER),
            sec_fetch_site: header_values(headers, &SEC_FETCH_SITE),
        };
        if let Err(error) = state.csrf.validate_mutation(
            request.method().as_str(),
            mutation_headers,
            &claims,
            owner_subject,
            authorization.canonical_origin(),
            (state.clock)(),
        ) {
            return ApiError::from_csrf(error, request_id).into_response();
        }
    }
    let mut response = next.run(request).await;
    response.headers_mut().insert(REQUEST_ID, request_id_header);
    response
}

pub fn prepare_request_id(request: &mut Request) -> Result<(RequestId, HeaderValue), ApiError> {
    let request_id = match resolve_request_id(request.headers()) {
        Ok(request_id) => request_id,
        Err(fallback) => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_request_id",
                "The request ID is invalid.",
                fallback,
            ));
        }
    };
    let request_id_header = match HeaderValue::from_str(&request_id) {
        Ok(value) => value,
        Err(_) => {
            let fallback = OperationId::new().to_string();
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_request_id",
                "The request ID is invalid.",
                fallback,
            ));
        }
    };
    request
        .headers_mut()
        .insert(REQUEST_ID.clone(), request_id_header.clone());
    let request_id = RequestId(request_id);
    request.extensions_mut().insert(request_id.clone());
    Ok((request_id, request_id_header))
}

pub fn shared_error_response(
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    request_id: RequestId,
) -> Response {
    ApiError::new(status, code, message, request_id.0).into_response()
}

fn resolve_request_id(headers: &HeaderMap) -> Result<String, String> {
    let fallback = || OperationId::new().to_string();
    let values: Vec<_> = headers.get_all(&REQUEST_ID).iter().collect();
    if values.is_empty() {
        return Ok(fallback());
    }
    if values.len() != 1 {
        return Err(fallback());
    }
    let value = values[0].to_str().map_err(|_| fallback())?;
    if value.is_empty()
        || value.len() > MAX_REQUEST_ID_BYTES
        || !value.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(fallback());
    }
    Ok(value.to_owned())
}

fn header_values<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Vec<&'a [u8]> {
    headers
        .get_all(name)
        .iter()
        .map(|value| value.as_bytes())
        .collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimBody {
    email: String,
    claim_code: String,
}

async fn claim_handler<S: EnrollmentStore + 'static>(
    State(state): State<SessionState<S>>,
    Extension(claims): Extension<AccessClaims>,
    Extension(request_id): Extension<RequestId>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    if body.len() > MAX_CLAIM_BODY_BYTES {
        return Err(ApiError::from_claim(
            ClaimRouteError::InvalidRequest,
            request_id.0,
        ));
    }
    let input: ClaimBody = serde_json::from_slice(&body)
        .map_err(|_| ApiError::from_claim(ClaimRouteError::InvalidRequest, request_id.0.clone()))?;
    let decoded = URL_SAFE_NO_PAD
        .decode(input.claim_code)
        .map_err(|_| ApiError::from_claim(ClaimRouteError::Forbidden, request_id.0.clone()))?;
    let code: [u8; 32] = decoded
        .try_into()
        .map_err(|_| ApiError::from_claim(ClaimRouteError::Forbidden, request_id.0.clone()))?;
    let content_types = header_values(&headers, &header::CONTENT_TYPE);
    let origins = header_values(&headers, &header::ORIGIN);
    claim_owner(
        &state.enrollment,
        &claims,
        &content_types,
        &origins,
        &input.email,
        &code,
        (state.clock)(),
    )
    .map_err(|error| ApiError::from_claim(error, request_id.0))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn session_handler<S: EnrollmentStore + 'static>(
    State(state): State<SessionState<S>>,
    Extension(claims): Extension<AccessClaims>,
    Extension(request_id): Extension<RequestId>,
    method: Method,
) -> Result<impl IntoResponse, ApiError> {
    let authorization = state
        .enrollment
        .authorize(RouteAccess::OwnerOnly, &claims)
        .map_err(|error| ApiError::from_enrollment(error, request_id.0.clone()))?;
    let owner_subject = authorization.owner_subject().ok_or_else(|| {
        ApiError::from_enrollment(EnrollmentError::Forbidden, request_id.0.clone())
    })?;
    get_session(
        method.as_str(),
        &state.csrf,
        &claims,
        owner_subject,
        (state.clock)(),
    )
    .map(|response| {
        (
            [
                (header::CACHE_CONTROL, "no-store"),
                (header::PRAGMA, "no-cache"),
            ],
            Json(response),
        )
    })
    .map_err(|error| ApiError::from_session(error, request_id.0))
}

async fn not_found(Extension(request_id): Extension<RequestId>) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "route_not_found",
        "The requested resource was not found.",
        request_id.0,
    )
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorBody {
    code: &'static str,
    message: &'static str,
    request_id: String,
    details: BTreeMap<String, String>,
}

pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    request_id: String,
}

impl ApiError {
    const fn new(
        status: StatusCode,
        code: &'static str,
        message: &'static str,
        request_id: String,
    ) -> Self {
        Self {
            status,
            code,
            message,
            request_id,
        }
    }

    fn from_enrollment(error: EnrollmentError, request_id: String) -> Self {
        let message = match error {
            EnrollmentError::InvalidIdentity => "The authenticated identity is invalid.",
            EnrollmentError::Forbidden => "The authenticated owner is not allowed.",
            EnrollmentError::NotFound => "The enrollment resource was not found.",
            EnrollmentError::Conflict => "The enrollment request conflicts with current state.",
            EnrollmentError::Unavailable => "The enrollment service is temporarily unavailable.",
        };
        Self::new(claim_status(&error), error.code(), message, request_id)
    }

    fn from_csrf(error: CsrfError, request_id: String) -> Self {
        let message = match error {
            CsrfError::Unauthenticated => "Authentication is required.",
            CsrfError::Forbidden => "The mutation request could not be authorized.",
            CsrfError::Capacity | CsrfError::Unavailable => {
                "The request security service is temporarily unavailable."
            }
        };
        Self::new(csrf_status(&error), error.code(), message, request_id)
    }

    fn from_claim(error: ClaimRouteError, request_id: String) -> Self {
        let message = match error {
            ClaimRouteError::InvalidRequest => "The owner claim request is invalid.",
            ClaimRouteError::UnsupportedMediaType => "The owner claim content type is unsupported.",
            ClaimRouteError::Forbidden => "The owner claim request is forbidden.",
            ClaimRouteError::Enrollment(enrollment) => {
                return Self::from_enrollment(enrollment, request_id);
            }
        };
        Self::new(error.status(), error.code(), message, request_id)
    }

    fn from_session(error: SessionRouteError, request_id: String) -> Self {
        match error {
            SessionRouteError::NotFound => Self::new(
                error.status(),
                error.code(),
                "The session resource was not found.",
                request_id,
            ),
            SessionRouteError::Csrf(csrf) => Self::from_csrf(csrf, request_id),
        }
    }
}

impl fmt::Debug for ApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            code: self.code,
            message: self.message,
            request_id: self.request_id.clone(),
            details: BTreeMap::new(),
        };
        (self.status, [(REQUEST_ID, self.request_id)], Json(body)).into_response()
    }
}
