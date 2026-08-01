use std::fmt;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Extension, Request, State};
use axum::http::{HeaderMap, HeaderName, Method, StatusCode, header};
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
use serde::{Deserialize, Serialize};

const CSRF_HEADER: HeaderName = HeaderName::from_static("x-cellar-csrf");
const SEC_FETCH_SITE: HeaderName = HeaderName::from_static("sec-fetch-site");
const MAX_CLAIM_BODY_BYTES: usize = 4 * 1024;

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
    request: Request,
    next: Next,
) -> Response {
    let Some(claims) = request.extensions().get::<AccessClaims>().cloned() else {
        return ApiError::new(StatusCode::UNAUTHORIZED, "missing_authentication").into_response();
    };
    let path = request.uri().path();
    let mode = match state.enrollment.mode() {
        Ok(mode) => mode,
        Err(error) => return ApiError::from_enrollment(error).into_response(),
    };
    let access = route_access(mode, path);
    let authorization = match state.enrollment.authorize(access, &claims) {
        Ok(authorization) => authorization,
        Err(error) => return ApiError::from_enrollment(error).into_response(),
    };
    if path != "/owner/claim" {
        let owner_subject = match authorization.owner_subject() {
            Some(subject) => subject,
            None => return ApiError::from_enrollment(EnrollmentError::Forbidden).into_response(),
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
            return ApiError::from_csrf(error).into_response();
        }
    }
    next.run(request).await
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
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    if body.len() > MAX_CLAIM_BODY_BYTES {
        return Err(ApiError::from_claim(ClaimRouteError::InvalidRequest));
    }
    let input: ClaimBody = serde_json::from_slice(&body)
        .map_err(|_| ApiError::from_claim(ClaimRouteError::InvalidRequest))?;
    let decoded = URL_SAFE_NO_PAD
        .decode(input.claim_code)
        .map_err(|_| ApiError::from_claim(ClaimRouteError::Forbidden))?;
    let code: [u8; 32] = decoded
        .try_into()
        .map_err(|_| ApiError::from_claim(ClaimRouteError::Forbidden))?;
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
    .map_err(ApiError::from_claim)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn session_handler<S: EnrollmentStore + 'static>(
    State(state): State<SessionState<S>>,
    Extension(claims): Extension<AccessClaims>,
    method: Method,
) -> Result<impl IntoResponse, ApiError> {
    let authorization = state
        .enrollment
        .authorize(RouteAccess::OwnerOnly, &claims)
        .map_err(ApiError::from_enrollment)?;
    let owner_subject = authorization
        .owner_subject()
        .ok_or_else(|| ApiError::from_enrollment(EnrollmentError::Forbidden))?;
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
    .map_err(ApiError::from_session)
}

async fn not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}

#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
}

pub struct ApiError {
    status: StatusCode,
    code: &'static str,
}

impl ApiError {
    const fn new(status: StatusCode, code: &'static str) -> Self {
        Self { status, code }
    }

    fn from_enrollment(error: EnrollmentError) -> Self {
        Self::new(claim_status(&error), error.code())
    }

    fn from_csrf(error: CsrfError) -> Self {
        Self::new(csrf_status(&error), error.code())
    }

    fn from_claim(error: ClaimRouteError) -> Self {
        Self::new(error.status(), error.code())
    }

    fn from_session(error: SessionRouteError) -> Self {
        Self::new(error.status(), error.code())
    }
}

impl fmt::Debug for ApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(ErrorBody { error: self.code })).into_response()
    }
}
