use std::collections::{HashMap, HashSet};
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::DecodingKey;
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{Mutex, Notify};
use tokio::time::{Instant, timeout};
use url::Url;

use crate::{AccessValidatorConfig, AuthError};

const MAX_JWK_FIELD_BYTES: usize = 16 * 1_024;
const MIN_RSA_MODULUS_BYTES: usize = 256;
const MAX_RSA_MODULUS_BYTES: usize = 1_024;
const MAX_RSA_EXPONENT_BYTES: usize = 8;

pub struct JwksResponse {
    status: u16,
    body: Pin<Box<dyn AsyncRead + Send>>,
    cache_max_age: Option<Duration>,
}

impl JwksResponse {
    pub fn new<R>(status: u16, body: R, cache_max_age: Option<Duration>) -> Self
    where
        R: AsyncRead + Send + 'static,
    {
        Self {
            status,
            body: Box::pin(body),
            cache_max_age,
        }
    }
}

impl fmt::Debug for JwksResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JwksResponse")
            .field("status", &self.status)
            .field("cache_max_age", &self.cache_max_age)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy)]
pub struct JwksFetchError {
    code: &'static str,
}

impl JwksFetchError {
    pub const fn unavailable() -> Self {
        Self {
            code: "jwks_fetch_unavailable",
        }
    }
}

impl fmt::Display for JwksFetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code)
    }
}

impl fmt::Debug for JwksFetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code)
    }
}

impl std::error::Error for JwksFetchError {}

#[async_trait]
pub trait JwksFetcher: Send + Sync + 'static {
    async fn fetch(&self, url: &Url) -> Result<JwksResponse, JwksFetchError>;
}

#[derive(Default)]
struct CacheState {
    keys: HashMap<String, Arc<DecodingKey>>,
    expires_at: Option<Instant>,
    refreshing: bool,
    generation: u64,
    last_attempt: Option<Instant>,
    last_result: Option<Result<(), AuthError>>,
}

pub(crate) struct JwksCache {
    config: AccessValidatorConfig,
    fetcher: Arc<dyn JwksFetcher>,
    state: Mutex<CacheState>,
    changed: Notify,
}

impl JwksCache {
    pub(crate) fn new(config: AccessValidatorConfig, fetcher: Arc<dyn JwksFetcher>) -> Arc<Self> {
        Arc::new(Self {
            config,
            fetcher,
            state: Mutex::new(CacheState::default()),
            changed: Notify::new(),
        })
    }

    pub(crate) async fn key(self: &Arc<Self>, kid: &str) -> Result<Arc<DecodingKey>, AuthError> {
        {
            let state = self.state.lock().await;
            if state
                .expires_at
                .is_some_and(|expires_at| expires_at > Instant::now())
                && let Some(key) = state.keys.get(kid)
            {
                return Ok(key.clone());
            }
        }

        self.refresh().await?;

        let state = self.state.lock().await;
        if state
            .expires_at
            .is_none_or(|expires_at| expires_at <= Instant::now())
        {
            return Err(AuthError::new("jwks_stale"));
        }
        state
            .keys
            .get(kid)
            .cloned()
            .ok_or_else(|| AuthError::new("unknown_kid"))
    }

    async fn refresh(self: &Arc<Self>) -> Result<(), AuthError> {
        let observed_generation;
        {
            let mut state = self.state.lock().await;
            observed_generation = state.generation;
            if !state.refreshing {
                let now = Instant::now();
                if state.last_attempt.is_some_and(|last_attempt| {
                    now.saturating_duration_since(last_attempt) < self.config.min_refresh_interval()
                }) {
                    return Err(AuthError::new("jwks_refresh_rate_limited"));
                }
                state.refreshing = true;
                state.last_attempt = Some(now);
                let cache = self.clone();
                tokio::spawn(async move {
                    let worker_cache = cache.clone();
                    let worker = tokio::spawn(async move { worker_cache.fetch_keys().await });
                    let result = match worker.await {
                        Ok(result) => result,
                        Err(_) => Err(AuthError::new("jwks_refresh_failed")),
                    };
                    cache.complete_refresh(result).await;
                });
            }
        }

        loop {
            let notified = self.changed.notified();
            {
                let state = self.state.lock().await;
                if state.generation != observed_generation {
                    return state
                        .last_result
                        .unwrap_or_else(|| Err(AuthError::new("jwks_refresh_failed")));
                }
            }
            notified.await;
        }
    }

    async fn complete_refresh(
        &self,
        result: Result<(HashMap<String, Arc<DecodingKey>>, Duration), AuthError>,
    ) {
        let mut state = self.state.lock().await;
        match result {
            Ok((keys, lifetime)) => {
                state.keys = keys;
                state.expires_at = Some(Instant::now() + lifetime);
                state.last_result = Some(Ok(()));
            }
            Err(error) => {
                state.last_result = Some(Err(error));
            }
        }
        state.refreshing = false;
        state.generation = state.generation.wrapping_add(1);
        drop(state);
        self.changed.notify_waiters();
    }

    async fn fetch_keys(&self) -> Result<(HashMap<String, Arc<DecodingKey>>, Duration), AuthError> {
        let operation = async {
            let mut response = self
                .fetcher
                .fetch(self.config.jwks_url())
                .await
                .map_err(|_| AuthError::new("jwks_refresh_failed"))?;
            if response.status != 200 {
                return Err(AuthError::new("jwks_refresh_failed"));
            }

            let mut body = Vec::new();
            let mut chunk = [0_u8; 8 * 1_024];
            loop {
                let read = response
                    .body
                    .read(&mut chunk)
                    .await
                    .map_err(|_| AuthError::new("jwks_refresh_failed"))?;
                if read == 0 {
                    break;
                }
                if body.len().saturating_add(read) > self.config.max_jwks_bytes() {
                    return Err(AuthError::new("jwks_response_too_large"));
                }
                body.extend_from_slice(&chunk[..read]);
            }

            let keys = parse_jwks(&body, self.config.max_jwks_keys())?;
            let lifetime = response
                .cache_max_age
                .unwrap_or_else(|| self.config.cache_ttl())
                .min(self.config.cache_ttl());
            Ok((keys, lifetime))
        };

        timeout(self.config.fetch_timeout(), operation)
            .await
            .map_err(|_| AuthError::new("jwks_timeout"))?
    }
}

#[derive(Deserialize)]
struct RawJwks {
    keys: Vec<RawJwk>,
}

#[derive(Deserialize)]
struct RawJwk {
    kty: String,
    #[serde(rename = "use")]
    key_use: Option<String>,
    alg: Option<String>,
    kid: String,
    n: Option<String>,
    e: Option<String>,
}

fn parse_jwks(
    body: &[u8],
    max_keys: usize,
) -> Result<HashMap<String, Arc<DecodingKey>>, AuthError> {
    let raw: RawJwks =
        serde_json::from_slice(body).map_err(|_| AuthError::new("jwks_malformed"))?;
    if raw.keys.len() > max_keys {
        return Err(AuthError::new("jwks_too_many_keys"));
    }

    let mut keys = HashMap::new();
    let mut seen_kids = HashSet::new();
    for key in raw.keys {
        if key.kid.is_empty() || key.kid.len() > MAX_JWK_FIELD_BYTES {
            return Err(AuthError::new("jwks_malformed"));
        }
        if key.kty.len() > MAX_JWK_FIELD_BYTES
            || key
                .key_use
                .as_ref()
                .is_some_and(|value| value.len() > MAX_JWK_FIELD_BYTES)
            || key
                .alg
                .as_ref()
                .is_some_and(|value| value.len() > MAX_JWK_FIELD_BYTES)
        {
            return Err(AuthError::new("jwks_malformed"));
        }
        if key.kty != "RSA"
            || key.key_use.as_deref() != Some("sig")
            || key.alg.as_deref() != Some("RS256")
        {
            continue;
        }
        if !seen_kids.insert(key.kid.clone()) {
            return Err(AuthError::new("jwks_malformed"));
        }
        let (Some(n), Some(e)) = (key.n, key.e) else {
            return Err(AuthError::new("jwks_malformed"));
        };
        if n.is_empty()
            || e.is_empty()
            || n.len() > MAX_JWK_FIELD_BYTES
            || e.len() > MAX_JWK_FIELD_BYTES
        {
            return Err(AuthError::new("jwks_malformed"));
        }
        let modulus = URL_SAFE_NO_PAD
            .decode(n.as_bytes())
            .map_err(|_| AuthError::new("jwks_malformed"))?;
        let exponent = URL_SAFE_NO_PAD
            .decode(e.as_bytes())
            .map_err(|_| AuthError::new("jwks_malformed"))?;
        if !(MIN_RSA_MODULUS_BYTES..=MAX_RSA_MODULUS_BYTES).contains(&modulus.len())
            || exponent.is_empty()
            || exponent.len() > MAX_RSA_EXPONENT_BYTES
            || modulus.first() == Some(&0)
            || modulus.last().is_none_or(|value| value & 1 == 0)
            || exponent.first() == Some(&0)
        {
            return Err(AuthError::new("jwks_malformed"));
        }
        let exponent_value = exponent
            .iter()
            .fold(0_u64, |value, byte| (value << 8) | u64::from(*byte));
        if exponent_value < 3 || exponent_value & 1 == 0 {
            return Err(AuthError::new("jwks_malformed"));
        }
        let decoding_key = DecodingKey::from_rsa_components(&n, &e)
            .map_err(|_| AuthError::new("jwks_malformed"))?;
        keys.insert(key.kid, Arc::new(decoding_key));
    }
    Ok(keys)
}
