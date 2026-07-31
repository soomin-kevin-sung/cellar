use std::fmt;
use std::sync::Mutex;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::AccessClaims;

const TOKEN_BYTES: usize = 32;
const MAX_SESSION_SECONDS: i64 = 8 * 60 * 60;

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum CsrfError {
    Unauthenticated,
    Forbidden,
    Unavailable,
}

impl CsrfError {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Unauthenticated => "csrf_unauthenticated",
            Self::Forbidden => "csrf_forbidden",
            Self::Unavailable => "csrf_unavailable",
        }
    }
}

impl fmt::Debug for CsrfError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl fmt::Display for CsrfError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for CsrfError {}

#[derive(Clone, Copy, Eq, PartialEq)]
struct AccessBinding {
    iat: i64,
    exp: i64,
    subject_hash: [u8; 32],
}

struct SessionRecord {
    binding: AccessBinding,
    token_hash: [u8; 32],
    issued_at: i64,
}

pub struct CsrfManager {
    current: Mutex<Option<SessionRecord>>,
}

impl Default for CsrfManager {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for CsrfManager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CsrfManager")
            .finish_non_exhaustive()
    }
}

impl CsrfManager {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            current: Mutex::new(None),
        }
    }

    pub fn issue(
        &self,
        claims: &AccessClaims,
        owner_subject: &str,
        now_unix_seconds: i64,
    ) -> Result<String, CsrfError> {
        let binding = authenticate(claims, owner_subject, now_unix_seconds)?;
        let mut token = [0_u8; TOKEN_BYTES];
        getrandom::fill(&mut token).map_err(|_| CsrfError::Unavailable)?;
        let token_hash = Sha256::digest(token).into();
        let encoded = URL_SAFE_NO_PAD.encode(token);
        let mut current = self.current.lock().map_err(|_| CsrfError::Unavailable)?;
        *current = Some(SessionRecord {
            binding,
            token_hash,
            issued_at: now_unix_seconds,
        });
        Ok(encoded)
    }

    pub fn validate_mutation(
        &self,
        method: &str,
        headers: MutationHeaders<'_>,
        claims: &AccessClaims,
        owner_subject: &str,
        canonical_origin: &str,
        now_unix_seconds: i64,
    ) -> Result<(), CsrfError> {
        if matches!(method, "GET" | "HEAD" | "OPTIONS") {
            return Ok(());
        }
        let binding = authenticate(claims, owner_subject, now_unix_seconds)?;
        if headers.origins.len() != 1
            || headers.origins[0] != canonical_origin.as_bytes()
            || headers.csrf_tokens.len() != 1
            || headers.sec_fetch_site.len() > 1
            || headers
                .sec_fetch_site
                .first()
                .is_some_and(|value| *value == b"cross-site")
        {
            return Err(CsrfError::Forbidden);
        }
        let encoded =
            std::str::from_utf8(headers.csrf_tokens[0]).map_err(|_| CsrfError::Forbidden)?;
        let decoded = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| CsrfError::Forbidden)?;
        let token: [u8; TOKEN_BYTES] = decoded.try_into().map_err(|_| CsrfError::Forbidden)?;
        let candidate_hash: [u8; 32] = Sha256::digest(token).into();
        let current = self.current.lock().map_err(|_| CsrfError::Unavailable)?;
        let record = current.as_ref().ok_or(CsrfError::Forbidden)?;
        if record.binding != binding
            || now_unix_seconds >= record.issued_at.saturating_add(MAX_SESSION_SECONDS)
            || !bool::from(record.token_hash.ct_eq(&candidate_hash))
        {
            return Err(CsrfError::Forbidden);
        }
        Ok(())
    }
}

fn authenticate(
    claims: &AccessClaims,
    owner_subject: &str,
    now_unix_seconds: i64,
) -> Result<AccessBinding, CsrfError> {
    if owner_subject.is_empty()
        || claims.sub != owner_subject
        || claims.sub.is_empty()
        || claims.iat > now_unix_seconds
        || claims.exp <= now_unix_seconds
        || claims.iat >= claims.exp
    {
        return Err(CsrfError::Unauthenticated);
    }
    Ok(AccessBinding {
        iat: claims.iat,
        exp: claims.exp,
        subject_hash: Sha256::digest(claims.sub.as_bytes()).into(),
    })
}

pub struct MutationHeaders<'a> {
    pub origins: Vec<&'a [u8]>,
    pub csrf_tokens: Vec<&'a [u8]>,
    pub sec_fetch_site: Vec<&'a [u8]>,
}

impl MutationHeaders<'_> {
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            origins: Vec::new(),
            csrf_tokens: Vec::new(),
            sec_fetch_site: Vec::new(),
        }
    }
}

impl fmt::Debug for MutationHeaders<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MutationHeaders")
            .field("origin_count", &self.origins.len())
            .field("csrf_token_count", &self.csrf_tokens.len())
            .field("sec_fetch_site_count", &self.sec_fetch_site.len())
            .finish()
    }
}
