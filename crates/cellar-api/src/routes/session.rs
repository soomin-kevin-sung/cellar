use axum::http::StatusCode;
use std::fmt;

use cellar_auth::{
    AccessClaims, ClaimRequest, CsrfError, CsrfManager, EnrollmentError, EnrollmentMode,
    EnrollmentService, EnrollmentStore,
};
use serde::Serialize;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionRouteError {
    NotFound,
    Csrf(CsrfError),
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClaimRouteError {
    UnsupportedMediaType,
    Forbidden,
    Enrollment(EnrollmentError),
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
