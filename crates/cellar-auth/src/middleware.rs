use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jsonwebtoken::errors::ErrorKind;
use jsonwebtoken::{Algorithm, Validation, decode, decode_header};
use url::Url;

use crate::claims::validate_claims;
use crate::jwks::JwksCache;
use crate::{AccessClaims, JwksFetcher, OwnerMode};

const DEFAULT_MAX_TOKEN_BYTES: usize = 16 * 1_024;
const ABSOLUTE_MAX_TOKEN_BYTES: usize = 64 * 1_024;
const DEFAULT_MAX_JWKS_BYTES: usize = 256 * 1_024;
const ABSOLUTE_MAX_JWKS_BYTES: usize = 1024 * 1_024;
const DEFAULT_MAX_JWKS_KEYS: usize = 32;
const ABSOLUTE_MAX_JWKS_KEYS: usize = 128;
const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(300);
const MAX_CACHE_TTL: Duration = Duration::from_secs(3_600);
const DEFAULT_FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const MAX_REFRESH_INTERVAL: Duration = Duration::from_secs(300);
const MAX_CLOCK_SKEW: Duration = Duration::from_secs(60);
const MAX_KID_BYTES: usize = 512;

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct AuthError {
    code: &'static str,
}

impl AuthError {
    pub(crate) const fn new(code: &'static str) -> Self {
        Self { code }
    }

    pub const fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code)
    }
}

impl fmt::Debug for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code)
    }
}

impl std::error::Error for AuthError {}

#[derive(Clone, Debug)]
pub struct AccessValidatorConfig {
    issuer: String,
    audience: String,
    jwks_url: Url,
    clock_skew: Duration,
    max_token_len: usize,
    max_jwks_bytes: usize,
    max_jwks_keys: usize,
    cache_ttl: Duration,
    fetch_timeout: Duration,
    min_refresh_interval: Duration,
}

impl AccessValidatorConfig {
    pub fn new(
        issuer: impl AsRef<str>,
        audience: impl Into<String>,
        jwks_url: impl AsRef<str>,
    ) -> Result<Self, AuthError> {
        let issuer = normalize_issuer(issuer.as_ref())?;
        let audience = audience.into();
        if audience.is_empty() || audience.len() > 2_048 {
            return Err(AuthError::new("invalid_auth_config"));
        }
        let jwks_url =
            Url::parse(jwks_url.as_ref()).map_err(|_| AuthError::new("invalid_auth_config"))?;
        let issuer_url = Url::parse(&issuer).map_err(|_| AuthError::new("invalid_auth_config"))?;
        if jwks_url.scheme() != "https"
            || jwks_url.host_str().is_none()
            || !jwks_url.username().is_empty()
            || jwks_url.password().is_some()
            || jwks_url.origin() != issuer_url.origin()
            || jwks_url.path() != "/cdn-cgi/access/certs"
            || jwks_url.query().is_some()
            || jwks_url.fragment().is_some()
        {
            return Err(AuthError::new("invalid_auth_config"));
        }

        Ok(Self {
            issuer,
            audience,
            jwks_url,
            clock_skew: Duration::from_secs(30),
            max_token_len: DEFAULT_MAX_TOKEN_BYTES,
            max_jwks_bytes: DEFAULT_MAX_JWKS_BYTES,
            max_jwks_keys: DEFAULT_MAX_JWKS_KEYS,
            cache_ttl: DEFAULT_CACHE_TTL,
            fetch_timeout: DEFAULT_FETCH_TIMEOUT,
            min_refresh_interval: DEFAULT_REFRESH_INTERVAL,
        })
    }

    #[must_use]
    pub fn with_clock_skew(mut self, value: Duration) -> Self {
        self.clock_skew = value.min(MAX_CLOCK_SKEW);
        self
    }

    #[must_use]
    pub fn with_max_token_len(mut self, value: usize) -> Self {
        self.max_token_len = value.clamp(1, ABSOLUTE_MAX_TOKEN_BYTES);
        self
    }

    #[must_use]
    pub fn with_max_jwks_bytes(mut self, value: usize) -> Self {
        self.max_jwks_bytes = value.clamp(1, ABSOLUTE_MAX_JWKS_BYTES);
        self
    }

    #[must_use]
    pub fn with_max_jwks_keys(mut self, value: usize) -> Self {
        self.max_jwks_keys = value.clamp(1, ABSOLUTE_MAX_JWKS_KEYS);
        self
    }

    #[must_use]
    pub fn with_cache_ttl(mut self, value: Duration) -> Self {
        self.cache_ttl = value.clamp(Duration::from_millis(1), MAX_CACHE_TTL);
        self
    }

    #[must_use]
    pub fn with_fetch_timeout(mut self, value: Duration) -> Self {
        self.fetch_timeout = value.clamp(Duration::from_millis(1), MAX_FETCH_TIMEOUT);
        self
    }

    #[must_use]
    pub fn with_min_refresh_interval(mut self, value: Duration) -> Self {
        self.min_refresh_interval = value.clamp(Duration::from_millis(1), MAX_REFRESH_INTERVAL);
        self
    }

    pub const fn max_token_len(&self) -> usize {
        self.max_token_len
    }

    pub(crate) fn issuer(&self) -> &str {
        &self.issuer
    }

    pub(crate) fn audience(&self) -> &str {
        &self.audience
    }

    pub(crate) const fn jwks_url(&self) -> &Url {
        &self.jwks_url
    }

    pub(crate) const fn clock_skew(&self) -> Duration {
        self.clock_skew
    }

    pub(crate) const fn max_jwks_bytes(&self) -> usize {
        self.max_jwks_bytes
    }

    pub(crate) const fn max_jwks_keys(&self) -> usize {
        self.max_jwks_keys
    }

    pub(crate) const fn cache_ttl(&self) -> Duration {
        self.cache_ttl
    }

    pub(crate) const fn fetch_timeout(&self) -> Duration {
        self.fetch_timeout
    }

    pub(crate) const fn min_refresh_interval(&self) -> Duration {
        self.min_refresh_interval
    }
}

fn normalize_issuer(value: &str) -> Result<String, AuthError> {
    if value.is_empty() || value.len() > 2_048 {
        return Err(AuthError::new("invalid_auth_config"));
    }
    let parsed = Url::parse(value).map_err(|_| AuthError::new("invalid_auth_config"))?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.path() != "/"
    {
        return Err(AuthError::new("invalid_auth_config"));
    }
    Ok(value.trim_end_matches('/').to_owned())
}

#[derive(Clone)]
pub struct AccessValidator {
    config: AccessValidatorConfig,
    cache: Arc<JwksCache>,
}

impl fmt::Debug for AccessValidator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccessValidator")
            .field("issuer", &self.config.issuer)
            .field("audience", &"[redacted]")
            .field("jwks_url", &self.config.jwks_url)
            .finish_non_exhaustive()
    }
}

impl AccessValidator {
    pub fn new(config: AccessValidatorConfig, fetcher: Arc<dyn JwksFetcher>) -> Self {
        let cache = JwksCache::new(config.clone(), fetcher);
        Self { config, cache }
    }

    pub async fn validate(
        &self,
        encoded_token: &str,
        owner_mode: OwnerMode<'_>,
    ) -> Result<AccessClaims, AuthError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| AuthError::new("clock_unavailable"))?
            .as_secs();
        let now = i64::try_from(now).map_err(|_| AuthError::new("clock_unavailable"))?;
        if encoded_token.len() > self.config.max_token_len {
            return Err(AuthError::new("access_token_too_large"));
        }
        if encoded_token.is_empty() || encoded_token.split('.').count() != 3 {
            return Err(AuthError::new("malformed_token"));
        }

        let header = decode_header(encoded_token).map_err(|_| AuthError::new("malformed_token"))?;
        if header.alg != Algorithm::RS256 {
            return Err(AuthError::new("invalid_algorithm"));
        }
        if header.typ.as_deref() != Some("JWT") {
            return Err(AuthError::new("invalid_typ"));
        }
        let kid = header.kid.ok_or_else(|| AuthError::new("invalid_kid"))?;
        if kid.is_empty() || kid.len() > MAX_KID_BYTES {
            return Err(AuthError::new("invalid_kid"));
        }

        let key = self.cache.key(&kid).await?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.validate_exp = false;
        validation.validate_nbf = false;
        validation.validate_aud = false;
        validation.required_spec_claims.clear();
        let claims = decode::<AccessClaims>(encoded_token, &key, &validation)
            .map_err(map_decode_error)?
            .claims;
        let skew = i64::try_from(self.config.clock_skew().as_secs())
            .map_err(|_| AuthError::new("invalid_auth_config"))?;
        validate_claims(
            &claims,
            self.config.issuer(),
            self.config.audience(),
            skew,
            now,
            owner_mode,
        )?;
        Ok(claims)
    }
}

fn map_decode_error(error: jsonwebtoken::errors::Error) -> AuthError {
    match error.kind() {
        ErrorKind::Json(_) | ErrorKind::MissingRequiredClaim(_) => {
            AuthError::new("malformed_claims")
        }
        _ => AuthError::new("invalid_signature"),
    }
}

pub fn select_access_jwt_header<'a>(
    values: impl IntoIterator<Item = &'a [u8]>,
    max_encoded_token_len: usize,
) -> Result<&'a str, AuthError> {
    let mut values = values.into_iter();
    let value = values
        .next()
        .ok_or_else(|| AuthError::new("missing_access_token"))?;
    if values.next().is_some() {
        return Err(AuthError::new("duplicate_access_token"));
    }
    if value.len() > max_encoded_token_len {
        return Err(AuthError::new("access_token_too_large"));
    }
    let value =
        std::str::from_utf8(value).map_err(|_| AuthError::new("invalid_access_token_header"))?;
    if value.is_empty() {
        return Err(AuthError::new("invalid_access_token_header"));
    }
    Ok(value)
}
