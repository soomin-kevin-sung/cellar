//! Authentication and authorization.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bytes::BytesMut;
use jsonwebtoken::{
    Algorithm, DecodingKey, Validation, decode, decode_header,
    jwk::{AlgorithmParameters, JwkSet, KeyAlgorithm, KeyOperations, PublicKeyUse},
};
use reqwest::{Client, Url, redirect::Policy};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::config::{AccessConfig, normalize_owner_email};

const MAX_ASSERTION_BYTES: usize = 16 * 1024;
const MAX_JWKS_BYTES: usize = 256 * 1024;
const JWKS_TTL: Duration = Duration::from_secs(60 * 60);
const UNKNOWN_KID_REFRESH_COOLDOWN: Duration = Duration::from_secs(30);
const FAILED_REFRESH_BACKOFF: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// The identity established by a verified Cloudflare Access assertion.
#[derive(Clone, PartialEq, Eq)]
pub struct OwnerIdentity {
    email: String,
}

impl OwnerIdentity {
    pub fn try_from_email(email: &str) -> Result<Self, AccessError> {
        let email = normalize_owner_email(email).map_err(|_| AccessError::unauthenticated())?;
        Ok(Self { email })
    }

    pub fn email(&self) -> &str {
        &self.email
    }
}

impl fmt::Debug for OwnerIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OwnerIdentity")
            .field("email", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessFailure {
    Unauthenticated,
    Forbidden,
    Unavailable,
}

/// A deliberately opaque authentication failure.
pub struct AccessError {
    classification: AccessFailure,
}

impl AccessError {
    pub fn unauthenticated() -> Self {
        Self {
            classification: AccessFailure::Unauthenticated,
        }
    }

    pub fn forbidden() -> Self {
        Self {
            classification: AccessFailure::Forbidden,
        }
    }

    pub fn unavailable() -> Self {
        Self {
            classification: AccessFailure::Unavailable,
        }
    }

    pub fn classification(&self) -> AccessFailure {
        self.classification
    }
}

impl fmt::Display for AccessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("access verification failed")
    }
}

impl fmt::Debug for AccessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let classification = match self.classification {
            AccessFailure::Unauthenticated => "unauthenticated",
            AccessFailure::Forbidden => "forbidden",
            AccessFailure::Unavailable => "unavailable",
        };
        formatter
            .debug_struct("AccessError")
            .field("classification", &classification)
            .finish()
    }
}

impl std::error::Error for AccessError {}

/// Narrow verification seam used by API middleware and handler tests.
pub trait AccessVerifier: Send + Sync {
    fn verify<'a>(
        &'a self,
        assertion: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<OwnerIdentity, AccessError>> + Send + 'a>>;
}

#[derive(Clone)]
pub struct CloudflareAccessVerifier {
    issuer: Arc<str>,
    audience: Arc<str>,
    owner_email: Arc<str>,
    jwks_url: Url,
    client: Client,
    cache_ttl: Duration,
    refresh_state: Arc<Mutex<RefreshState>>,
    refresh_lock: Arc<Mutex<()>>,
}

#[derive(Default)]
struct RefreshState {
    generation: u64,
    cache: Option<CachedKeys>,
    last_unknown_kid_refresh: Option<Instant>,
    last_refresh_outcome: Option<RefreshOutcome>,
    retry_not_before: Option<Instant>,
}

#[derive(Clone, Copy)]
enum RefreshOutcome {
    Successful,
    Unavailable,
}

struct CachedKeys {
    fetched_at: Instant,
    keys: HashMap<String, DecodingKey>,
}

#[derive(Deserialize)]
struct AccessClaims {
    iss: String,
    aud: AudienceClaim,
    exp: i64,
    nbf: i64,
    #[serde(rename = "type")]
    token_type: String,
    email: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum AudienceClaim {
    One(String),
    Many(Vec<String>),
}

impl AudienceClaim {
    fn contains(&self, expected: &str) -> bool {
        match self {
            Self::One(value) => value == expected,
            Self::Many(values) => values.iter().any(|value| value == expected),
        }
    }
}

impl CloudflareAccessVerifier {
    pub fn new(config: &AccessConfig) -> Result<Self, AccessError> {
        let jwks_url = format!("{}/cdn-cgi/access/certs", config.team_domain().as_str())
            .parse::<Url>()
            .map_err(|_| AccessError::unavailable())?;
        debug_assert_eq!(jwks_url.scheme(), "https");
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(Policy::none())
            .build()
            .map_err(|_| AccessError::unavailable())?;
        Ok(Self::from_parts(config, jwks_url, client, JWKS_TTL))
    }

    fn from_parts(
        config: &AccessConfig,
        jwks_url: Url,
        client: Client,
        cache_ttl: Duration,
    ) -> Self {
        Self {
            issuer: Arc::from(config.team_domain().as_str()),
            audience: Arc::from(config.audience()),
            owner_email: Arc::from(config.owner_email()),
            jwks_url,
            client,
            cache_ttl,
            refresh_state: Arc::new(Mutex::new(RefreshState::default())),
            refresh_lock: Arc::new(Mutex::new(())),
        }
    }

    #[cfg(test)]
    fn new_for_test(
        config: &AccessConfig,
        jwks_url: Url,
        client: Client,
        cache_ttl: Duration,
    ) -> Self {
        Self::from_parts(config, jwks_url, client, cache_ttl)
    }

    async fn verify_assertion(&self, assertion: &str) -> Result<OwnerIdentity, AccessError> {
        if assertion.is_empty() || assertion.len() > MAX_ASSERTION_BYTES {
            return Err(AccessError::unauthenticated());
        }

        let header = decode_header(assertion).map_err(|_| AccessError::unauthenticated())?;
        if header.alg != Algorithm::RS256 {
            return Err(AccessError::unauthenticated());
        }
        let kid = header
            .kid
            .filter(|kid| !kid.is_empty())
            .ok_or_else(AccessError::unauthenticated)?;
        let decoding_key = self.key_for(&kid).await?;

        // Zero leeway is deliberate: origin and Access share a clock source in
        // deployment, and fail-closed behavior is preferred at token boundaries.
        let mut validation = Validation::new(Algorithm::RS256);
        validation.leeway = 0;
        validation.validate_nbf = true;
        validation.validate_aud = false;
        validation.set_required_spec_claims(&["exp", "nbf", "iss", "aud"]);
        let claims = decode::<AccessClaims>(assertion, &decoding_key, &validation)
            .map_err(|_| AccessError::unauthenticated())?
            .claims;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| AccessError::unavailable())?
            .as_secs();
        let now = i64::try_from(now).map_err(|_| AccessError::unavailable())?;
        validate_temporal_claims(claims.exp, claims.nbf, now)?;
        if claims.iss != self.issuer.as_ref()
            || !claims.aud.contains(&self.audience)
            || claims.token_type != "app"
        {
            return Err(AccessError::forbidden());
        }
        let identity = OwnerIdentity::try_from_email(&claims.email)?;
        if identity.email() != self.owner_email.as_ref() {
            return Err(AccessError::forbidden());
        }
        Ok(identity)
    }

    async fn key_for(&self, kid: &str) -> Result<DecodingKey, AccessError> {
        let observed_generation = {
            let state = self.refresh_state.lock().await;
            if let Some(key) = fresh_key(&state, kid, self.cache_ttl) {
                return Ok(key);
            }
            if state
                .retry_not_before
                .is_some_and(|instant| Instant::now() < instant)
            {
                return Err(AccessError::unavailable());
            }
            if cache_is_fresh(&state, self.cache_ttl)
                && state
                    .last_unknown_kid_refresh
                    .is_some_and(|instant| instant.elapsed() < UNKNOWN_KID_REFRESH_COOLDOWN)
            {
                return Err(AccessError::unauthenticated());
            }
            state.generation
        };

        // This mutex is the single-flight boundary. Cache state is not held
        // across I/O, allowing concurrent callers to observe an active refresh.
        let _refresh_guard = self.refresh_lock.lock().await;
        let state = self.refresh_state.lock().await;
        if state.generation != observed_generation {
            return match state.last_refresh_outcome {
                Some(RefreshOutcome::Unavailable) => Err(AccessError::unavailable()),
                Some(RefreshOutcome::Successful) | None => {
                    fresh_key(&state, kid, self.cache_ttl).ok_or_else(AccessError::unauthenticated)
                }
            };
        }
        drop(state);

        let fetched = self.fetch_keys().await;
        let mut state = self.refresh_state.lock().await;
        state.generation = state.generation.wrapping_add(1);
        match fetched {
            Ok(keys) => {
                let result = keys.get(kid).cloned();
                state.last_refresh_outcome = Some(RefreshOutcome::Successful);
                state.retry_not_before = None;
                state.last_unknown_kid_refresh = result.is_none().then(Instant::now);
                state.cache = Some(CachedKeys {
                    fetched_at: Instant::now(),
                    keys,
                });
                result.ok_or_else(AccessError::unauthenticated)
            }
            Err(error) => {
                state.last_refresh_outcome = Some(RefreshOutcome::Unavailable);
                state.retry_not_before = Some(Instant::now() + FAILED_REFRESH_BACKOFF);
                Err(error)
            }
        }
    }

    async fn fetch_keys(&self) -> Result<HashMap<String, DecodingKey>, AccessError> {
        let mut response = self
            .client
            .get(self.jwks_url.clone())
            .send()
            .await
            .map_err(|_| AccessError::unavailable())?;
        if !response.status().is_success() {
            return Err(AccessError::unavailable());
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_JWKS_BYTES as u64)
        {
            return Err(AccessError::unavailable());
        }

        let mut body = BytesMut::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| AccessError::unavailable())?
        {
            if body.len().saturating_add(chunk.len()) > MAX_JWKS_BYTES {
                return Err(AccessError::unavailable());
            }
            body.extend_from_slice(&chunk);
        }
        let jwks: JwkSet = serde_json::from_slice(&body).map_err(|_| AccessError::unavailable())?;

        let mut keys = HashMap::new();
        let mut duplicate_ids = HashSet::new();
        for jwk in jwks.keys {
            let Some(kid) = jwk.common.key_id.clone().filter(|kid| !kid.is_empty()) else {
                continue;
            };
            let key_ops_valid = jwk.common.key_operations.as_ref().is_none_or(|operations| {
                !operations.is_empty()
                    && operations
                        .iter()
                        .all(|operation| *operation == KeyOperations::Verify)
            });
            let usable = jwk.common.public_key_use == Some(PublicKeyUse::Signature)
                && jwk.common.key_algorithm == Some(KeyAlgorithm::RS256)
                && key_ops_valid
                && matches!(jwk.algorithm, AlgorithmParameters::RSA(_));
            if !usable {
                continue;
            }
            let Ok(key) = DecodingKey::from_jwk(&jwk) else {
                continue;
            };
            if keys.insert(kid.clone(), key).is_some() {
                duplicate_ids.insert(kid);
            }
        }
        for kid in duplicate_ids {
            keys.remove(&kid);
        }
        if keys.is_empty() {
            return Err(AccessError::unavailable());
        }
        Ok(keys)
    }
}

fn validate_temporal_claims(exp: i64, nbf: i64, now: i64) -> Result<(), AccessError> {
    if exp <= now || nbf > now {
        Err(AccessError::unauthenticated())
    } else {
        Ok(())
    }
}

fn fresh_key(state: &RefreshState, kid: &str, ttl: Duration) -> Option<DecodingKey> {
    if !cache_is_fresh(state, ttl) {
        return None;
    }
    let cache = state.cache.as_ref()?;
    cache.keys.get(kid).cloned()
}

fn cache_is_fresh(state: &RefreshState, ttl: Duration) -> bool {
    state
        .cache
        .as_ref()
        .is_some_and(|cache| cache.fetched_at.elapsed() < ttl)
}

impl AccessVerifier for CloudflareAccessVerifier {
    fn verify<'a>(
        &'a self,
        assertion: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<OwnerIdentity, AccessError>> + Send + 'a>> {
        Box::pin(async move { self.verify_assertion(assertion).await })
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    use reqwest::Client;
    use serde_json::{Value, json};
    use time::OffsetDateTime;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    use super::{
        AccessFailure, AccessVerifier, CloudflareAccessVerifier, MAX_JWKS_BYTES,
        validate_temporal_claims,
    };
    use crate::config::{AccessConfig, Config};

    const PRIVATE_KEY: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDJETqse41HRBsc
7cfcq3ak4oZWFCoZlcic525A3FfO4qW9BMtRO/iXiyCCHn8JhiL9y8j5JdVP2Q9Z
IpfElcFd3/guS9w+5RqQGgCR+H56IVUyHZWtTJbKPcwWXQdNUX0rBFcsBzCRESJL
eelOEdHIjG7LRkx5l/FUvlqsyHDVJEQsHwegZ8b8C0fz0EgT2MMEdn10t6Ur1rXz
jMB/wvCg8vG8lvciXmedyo9xJ8oMOh0wUEgxziVDMMovmC+aJctcHUAYubwoGN8T
yzcvnGqL7JSh36Pwy28iPzXZ2RLhAyJFU39vLaHdljwthUaupldlNyCfa6Ofy4qN
ctlUPlN1AgMBAAECggEAdESTQjQ70O8QIp1ZSkCYXeZjuhj081CK7jhhp/4ChK7J
GlFQZMwiBze7d6K84TwAtfQGZhQ7km25E1kOm+3hIDCoKdVSKch/oL54f/BK6sKl
qlIzQEAenho4DuKCm3I4yAw9gEc0DV70DuMTR0LEpYyXcNJY3KNBOTjN5EYQAR9s
2MeurpgK2MdJlIuZaIbzSGd+diiz2E6vkmcufJLtmYUT/k/ddWvEtz+1DnO6bRHh
xuuDMeJA/lGB/EYloSLtdyCF6sII6C6slJJtgfb0bPy7l8VtL5iDyz46IKyzdyzW
tKAn394dm7MYR1RlUBEfqFUyNK7C+pVMVoTwCC2V4QKBgQD64syfiQ2oeUlLYDm4
CcKSP3RnES02bcTyEDFSuGyyS1jldI4A8GXHJ/lG5EYgiYa1RUivge4lJrlNfjyf
dV230xgKms7+JiXqag1FI+3mqjAgg4mYiNjaao8N8O3/PD59wMPeWYImsWXNyeHS
55rUKiHERtCcvdzKl4u35ZtTqQKBgQDNKnX2bVqOJ4WSqCgHRhOm386ugPHfy+8j
m6cicmUR46ND6ggBB03bCnEG9OtGisxTo/TuYVRu3WP4KjoJs2LD5fwdwJqpgtHl
yVsk45Y1Hfo+7M6lAuR8rzCi6kHHNb0HyBmZjysHWZsn79ZM+sQnLpgaYgQGRbKV
DZWlbw7g7QKBgQCl1u+98UGXAP1jFutwbPsx40IVszP4y5ypCe0gqgon3UiY/G+1
zTLp79GGe/SjI2VpQ7AlW7TI2A0bXXvDSDi3/5Dfya9ULnFXv9yfvH1QwWToySpW
Kvd1gYSoiX84/WCtjZOr0e0HmLIb0vw0hqZA4szJSqoxQgvF22EfIWaIaQKBgQCf
34+OmMYw8fEvSCPxDxVvOwW2i7pvV14hFEDYIeZKW2W1HWBhVMzBfFB5SE8yaCQy
pRfOzj9aKOCm2FjjiErVNpkQoi6jGtLvScnhZAt/lr2TXTrl8OwVkPrIaN0bG/AS
aUYxmBPCpXu3UjhfQiWqFq/mFyzlqlgvuCc9g95HPQKBgAscKP8mLxdKwOgX8yFW
GcZ0izY/30012ajdHY+/QK5lsMoxTnn0skdS+spLxaS5ZEO4qvPVb8RAoCkWMMal
2pOhmquJQVDPDLuZHdrIiKiDM20dy9sMfHygWcZjQ4WSxf/J7T9canLZIXFhHAZT
3wc9h4G8BBCtWN2TN/LsGZdB
-----END PRIVATE KEY-----"#;
    const MODULUS: &str = "yRE6rHuNR0QbHO3H3Kt2pOKGVhQqGZXInOduQNxXzuKlvQTLUTv4l4sggh5_CYYi_cvI-SXVT9kPWSKXxJXBXd_4LkvcPuUakBoAkfh-eiFVMh2VrUyWyj3MFl0HTVF9KwRXLAcwkREiS3npThHRyIxuy0ZMeZfxVL5arMhw1SRELB8HoGfG_AtH89BIE9jDBHZ9dLelK9a184zAf8LwoPLxvJb3Il5nncqPcSfKDDodMFBIMc4lQzDKL5gvmiXLXB1AGLm8KBjfE8s3L5xqi-yUod-j8MtvIj812dkS4QMiRVN_by2h3ZY8LYVGrqZXZTcgn2ujn8uKjXLZVD5TdQ";

    fn config() -> Config {
        Config::parse(
            r#"bind = "127.0.0.1:8787"
external_origin = "https://files.example.com"
data_root = "D:/CellarData"
database_path = "D:/CellarData/.cellar/cellar.db"

[access]
team_domain = "https://example.cloudflareaccess.com"
audience = "test-audience"
owner_email = "owner@example.com"
"#,
        )
        .unwrap()
    }

    fn jwks(kid: &str) -> Value {
        json!({"keys": [{
            "kty": "RSA", "use": "sig", "alg": "RS256", "kid": kid,
            "n": MODULUS, "e": "AQAB"
        }]})
    }

    fn claims() -> Value {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        json!({
            "iss": "https://example.cloudflareaccess.com",
            "aud": "test-audience",
            "exp": now + 300,
            "nbf": now - 1,
            "type": "app",
            "email": " Owner@Example.COM "
        })
    }

    fn token_with(claims: &Value, kid: Option<&str>, algorithm: Algorithm) -> String {
        let mut header = Header::new(algorithm);
        header.kid = kid.map(str::to_owned);
        let key = match algorithm {
            Algorithm::RS256 => EncodingKey::from_rsa_pem(PRIVATE_KEY.as_bytes()).unwrap(),
            Algorithm::HS256 => EncodingKey::from_secret(b"not-an-rsa-key"),
            _ => unreachable!(),
        };
        encode(&header, claims, &key).unwrap()
    }

    fn verifier(
        access: &AccessConfig,
        server: &MockServer,
        ttl: Duration,
    ) -> CloudflareAccessVerifier {
        CloudflareAccessVerifier::new_for_test(
            access,
            format!("{}/cdn-cgi/access/certs", server.uri())
                .parse()
                .unwrap(),
            Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            ttl,
        )
    }

    async fn mount_json(server: &MockServer, body: Value) {
        Mock::given(method("GET"))
            .and(path("/cdn-cgi/access/certs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    #[test]
    fn production_constructor_derives_the_exact_https_team_jwks_endpoint() {
        let config = config();
        let verifier = CloudflareAccessVerifier::new(config.access()).unwrap();

        assert_eq!(
            verifier.jwks_url.as_str(),
            "https://example.cloudflareaccess.com/cdn-cgi/access/certs"
        );
    }

    #[test]
    fn exact_expiry_and_future_not_before_are_unauthenticated() {
        let now = 1_700_000_000;

        for (exp, nbf) in [(now, now), (now + 300, now + 1)] {
            assert_eq!(
                validate_temporal_claims(exp, nbf, now)
                    .unwrap_err()
                    .classification(),
                AccessFailure::Unauthenticated
            );
        }
        assert!(validate_temporal_claims(now + 1, now, now).is_ok());
    }

    #[tokio::test]
    async fn verifies_real_rs256_identity_and_audience_array_without_requiring_iat() {
        let server = MockServer::start().await;
        mount_json(&server, jwks("key-1")).await;
        let config = config();
        let verifier = verifier(config.access(), &server, Duration::from_secs(3600));

        let identity = verifier
            .verify(&token_with(&claims(), Some("key-1"), Algorithm::RS256))
            .await
            .unwrap();
        assert_eq!(identity.email(), "owner@example.com");

        let mut array_claims = claims();
        array_claims["aud"] = json!(["other-audience", "test-audience"]);
        assert!(
            verifier
                .verify(&token_with(&array_claims, Some("key-1"), Algorithm::RS256))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn classifies_invalid_headers_signatures_and_time_claims_as_unauthenticated() {
        let server = MockServer::start().await;
        mount_json(&server, jwks("key-1")).await;
        let config = config();
        let verifier = verifier(config.access(), &server, Duration::from_secs(3600));
        let valid = token_with(&claims(), Some("key-1"), Algorithm::RS256);
        let (signing_input, signature) = valid.rsplit_once('.').unwrap();
        let replacement = if signature.starts_with('A') { "B" } else { "A" };
        let bad_signature = format!("{signing_input}.{replacement}{}", &signature[1..]);

        let mut cases = vec![
            ("bad signature", bad_signature),
            (
                "wrong algorithm",
                token_with(&claims(), Some("key-1"), Algorithm::HS256),
            ),
            ("missing kid", token_with(&claims(), None, Algorithm::RS256)),
            (
                "unknown kid",
                token_with(&claims(), Some("unknown"), Algorithm::RS256),
            ),
        ];
        for (label, field, value) in [
            (
                "expired",
                "exp",
                json!(OffsetDateTime::now_utc().unix_timestamp() - 1),
            ),
            (
                "future nbf",
                "nbf",
                json!(OffsetDateTime::now_utc().unix_timestamp() + 30),
            ),
        ] {
            let mut changed = claims();
            changed[field] = value;
            cases.push((label, token_with(&changed, Some("key-1"), Algorithm::RS256)));
        }
        let mut service_token = claims();
        service_token.as_object_mut().unwrap().remove("email");
        cases.push((
            "service token without email",
            token_with(&service_token, Some("key-1"), Algorithm::RS256),
        ));
        for field in ["exp", "nbf"] {
            let mut missing_integer_claim = claims();
            missing_integer_claim.as_object_mut().unwrap().remove(field);
            cases.push((
                "missing integer time claim",
                token_with(&missing_integer_claim, Some("key-1"), Algorithm::RS256),
            ));
        }
        let mut fractional_expiry = claims();
        fractional_expiry["exp"] = json!(1.5);
        cases.push((
            "fractional expiry",
            token_with(&fractional_expiry, Some("key-1"), Algorithm::RS256),
        ));

        for (label, token) in cases {
            assert_eq!(
                verifier.verify(&token).await.unwrap_err().classification(),
                AccessFailure::Unauthenticated,
                "case {label}"
            );
        }
        for token in ["".to_owned(), "x".repeat(16 * 1024 + 1)] {
            assert_eq!(
                verifier.verify(&token).await.unwrap_err().classification(),
                AccessFailure::Unauthenticated
            );
        }
    }

    #[tokio::test]
    async fn classifies_valid_signed_authorization_mismatches_as_forbidden() {
        let server = MockServer::start().await;
        mount_json(&server, jwks("key-1")).await;
        let config = config();
        let verifier = verifier(config.access(), &server, Duration::from_secs(3600));

        for (label, field, value) in [
            ("issuer", "iss", json!("https://other.cloudflareaccess.com")),
            ("audience", "aud", json!(["other-audience"])),
            ("owner", "email", json!("attacker@example.com")),
            ("token type", "type", json!("org")),
        ] {
            let mut changed = claims();
            changed[field] = value;
            let token = token_with(&changed, Some("key-1"), Algorithm::RS256);
            assert_eq!(
                verifier.verify(&token).await.unwrap_err().classification(),
                AccessFailure::Forbidden,
                "case {label}"
            );
        }
    }

    #[tokio::test]
    async fn jwks_failures_fail_closed() {
        let config = config();
        let token = token_with(&claims(), Some("key-1"), Algorithm::RS256);

        for (label, response) in [
            ("non-success", ResponseTemplate::new(503)),
            (
                "malformed",
                ResponseTemplate::new(200).set_body_string("not-json"),
            ),
            (
                "oversized",
                ResponseTemplate::new(200).set_body_string("x".repeat(MAX_JWKS_BYTES + 1)),
            ),
            (
                "unusable",
                ResponseTemplate::new(200).set_body_json(json!({"keys": [{
                    "kty": "oct", "use": "sig", "alg": "HS256", "kid": "key-1", "k": "AA"
                }]})),
            ),
            (
                "duplicate usable kid",
                ResponseTemplate::new(200).set_body_json(json!({"keys": [
                    {
                        "kty": "RSA", "use": "sig", "alg": "RS256", "kid": "key-1",
                        "n": MODULUS, "e": "AQAB"
                    },
                    {
                        "kty": "RSA", "use": "sig", "alg": "RS256", "kid": "key-1",
                        "n": MODULUS, "e": "AQAB"
                    }
                ]})),
            ),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/cdn-cgi/access/certs"))
                .respond_with(response)
                .mount(&server)
                .await;
            let verifier = verifier(config.access(), &server, Duration::from_secs(3600));
            let error = verifier.verify(&token).await.unwrap_err();
            assert_eq!(error.classification(), AccessFailure::Unavailable);
            assert_eq!(
                error.to_string(),
                "access verification failed",
                "case {label}"
            );
            let rendered = format!("{error:?}");
            assert!(!rendered.contains(&server.uri()), "case {label}");
            assert!(!rendered.contains("not-json"), "case {label}");
        }
    }

    #[tokio::test]
    async fn jwks_timeout_is_unavailable_without_leaking_details() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cdn-cgi/access/certs"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(100))
                    .set_body_json(jwks("key-1")),
            )
            .mount(&server)
            .await;
        let config = config();
        let verifier = CloudflareAccessVerifier::new_for_test(
            config.access(),
            format!("{}/cdn-cgi/access/certs", server.uri())
                .parse()
                .unwrap(),
            Client::builder()
                .timeout(Duration::from_millis(20))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            Duration::from_secs(3600),
        );
        let token = token_with(&claims(), Some("key-1"), Algorithm::RS256);

        let error = verifier.verify(&token).await.unwrap_err();
        assert_eq!(error.classification(), AccessFailure::Unavailable);
        assert_eq!(error.to_string(), "access verification failed");
        assert!(!format!("{error:?}").contains(&server.uri()));
    }

    #[tokio::test]
    async fn known_key_cache_avoids_second_fetch_and_unknown_kid_refreshes_once() {
        let server = MockServer::start().await;
        mount_json(&server, jwks("key-1")).await;
        let config = config();
        let verifier = verifier(config.access(), &server, Duration::from_secs(3600));
        let known = token_with(&claims(), Some("key-1"), Algorithm::RS256);

        assert!(verifier.verify(&known).await.is_ok());
        assert!(verifier.verify(&known).await.is_ok());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);

        let unknown = token_with(&claims(), Some("unknown"), Algorithm::RS256);
        assert_eq!(
            verifier
                .verify(&unknown)
                .await
                .unwrap_err()
                .classification(),
            AccessFailure::Unauthenticated
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn sequential_unknown_kids_cannot_force_unbounded_refreshes() {
        let server = MockServer::start().await;
        mount_json(&server, jwks("key-1")).await;
        let config = config();
        let verifier = verifier(config.access(), &server, Duration::from_secs(3600));
        let known = token_with(&claims(), Some("key-1"), Algorithm::RS256);
        assert!(verifier.verify(&known).await.is_ok());

        for kid in ["unknown-1", "unknown-2", "unknown-3"] {
            let unknown = token_with(&claims(), Some(kid), Algorithm::RS256);
            assert!(verifier.verify(&unknown).await.is_err());
        }

        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn cold_successful_unknown_kid_does_not_immediately_refetch() {
        let server = MockServer::start().await;
        mount_json(&server, jwks("key-1")).await;
        let config = config();
        let verifier = verifier(config.access(), &server, Duration::from_secs(3600));

        for kid in ["unknown-1", "unknown-2"] {
            let token = token_with(&claims(), Some(kid), Algorithm::RS256);
            assert_eq!(
                verifier.verify(&token).await.unwrap_err().classification(),
                AccessFailure::Unauthenticated
            );
        }

        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unknown_kid_refresh_accepts_rotated_key() {
        let server = MockServer::start().await;
        mount_json(&server, jwks("key-1")).await;
        let config = config();
        let verifier = verifier(config.access(), &server, Duration::from_secs(3600));
        let first = token_with(&claims(), Some("key-1"), Algorithm::RS256);
        assert!(verifier.verify(&first).await.is_ok());

        server.reset().await;
        mount_json(&server, jwks("key-2")).await;
        let rotated = token_with(&claims(), Some("key-2"), Algorithm::RS256);
        assert!(verifier.verify(&rotated).await.is_ok());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn concurrent_cold_fetch_failure_is_shared_and_backed_off() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cdn-cgi/access/certs"))
            .respond_with(ResponseTemplate::new(503).set_delay(Duration::from_millis(100)))
            .mount(&server)
            .await;
        let config = config();
        let verifier = Arc::new(verifier(
            config.access(),
            &server,
            Duration::from_secs(3600),
        ));
        let token = Arc::new(token_with(&claims(), Some("key-1"), Algorithm::RS256));

        let tasks = (0..12).map(|_| {
            let verifier = verifier.clone();
            let token = token.clone();
            tokio::spawn(async move { verifier.verify(&token).await })
        });
        for result in futures_util::future::join_all(tasks).await {
            assert_eq!(
                result.unwrap().unwrap_err().classification(),
                AccessFailure::Unavailable
            );
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 1);

        assert_eq!(
            verifier.verify(&token).await.unwrap_err().classification(),
            AccessFailure::Unavailable
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn concurrent_cold_verification_uses_single_jwks_fetch() {
        let server = MockServer::start().await;
        mount_json(&server, jwks("key-1")).await;
        let config = config();
        let verifier = Arc::new(verifier(
            config.access(),
            &server,
            Duration::from_secs(3600),
        ));
        let token = Arc::new(token_with(&claims(), Some("key-1"), Algorithm::RS256));

        let tasks = (0..12).map(|_| {
            let verifier = verifier.clone();
            let token = token.clone();
            tokio::spawn(async move { verifier.verify(&token).await })
        });
        for result in futures_util::future::join_all(tasks).await {
            assert!(result.unwrap().is_ok());
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn concurrent_unknown_kid_verification_uses_one_forced_refresh() {
        let server = MockServer::start().await;
        mount_json(&server, jwks("key-1")).await;
        let config = config();
        let verifier = Arc::new(verifier(
            config.access(),
            &server,
            Duration::from_secs(3600),
        ));
        let known = token_with(&claims(), Some("key-1"), Algorithm::RS256);
        assert!(verifier.verify(&known).await.is_ok());

        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/cdn-cgi/access/certs"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(100))
                    .set_body_json(jwks("key-1")),
            )
            .mount(&server)
            .await;
        let unknown = Arc::new(token_with(&claims(), Some("rotated-key"), Algorithm::RS256));
        let tasks = (0..12).map(|_| {
            let verifier = verifier.clone();
            let unknown = unknown.clone();
            tokio::spawn(async move { verifier.verify(&unknown).await })
        });
        for result in futures_util::future::join_all(tasks).await {
            assert!(result.unwrap().is_err());
        }

        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn expired_cache_is_not_used_when_refresh_fails() {
        let server = MockServer::start().await;
        mount_json(&server, jwks("key-1")).await;
        let config = config();
        let verifier = Arc::new(verifier(config.access(), &server, Duration::ZERO));
        let token = Arc::new(token_with(&claims(), Some("key-1"), Algorithm::RS256));
        assert!(verifier.verify(&token).await.is_ok());

        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/cdn-cgi/access/certs"))
            .respond_with(ResponseTemplate::new(503).set_delay(Duration::from_millis(100)))
            .mount(&server)
            .await;
        let tasks = (0..12).map(|_| {
            let verifier = verifier.clone();
            let token = token.clone();
            tokio::spawn(async move { verifier.verify(&token).await })
        });
        for result in futures_util::future::join_all(tasks).await {
            assert_eq!(
                result.unwrap().unwrap_err().classification(),
                AccessFailure::Unavailable
            );
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        assert_eq!(
            verifier.verify(&token).await.unwrap_err().classification(),
            AccessFailure::Unavailable
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}
