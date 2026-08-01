use std::collections::VecDeque;
use std::fmt;
use std::io::Cursor;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cellar_auth::{
    AccessClaims, AccessValidator, AccessValidatorConfig, JwksFetchError, JwksFetcher,
    JwksResponse, OwnerMode, select_access_jwt_header,
};
use cellar_config::MAX_AUD_TAGS;
use jsonwebtoken::crypto::sign;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use rsa::pkcs8::{EncodePrivateKey, LineEnding};
use rsa::rand_core::OsRng;
use rsa::traits::PublicKeyParts;
use rsa::{RsaPrivateKey, RsaPublicKey};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::{Notify, Semaphore};
use url::Url;

const ISSUER: &str = "https://cellar.cloudflareaccess.com";
const AUDIENCE: &str = "cellar-audience";
const OWNER_SUBJECT: &str = "owner-subject";
const OWNER_EMAIL: &str = "owner@example.com";

struct TestKey {
    kid: &'static str,
    private_pem: String,
    n: String,
    e: String,
}

impl TestKey {
    fn generate(kid: &'static str) -> Self {
        let private = RsaPrivateKey::new(&mut OsRng, 2048).expect("generate RSA test key");
        let public = RsaPublicKey::from(&private);
        Self {
            kid,
            private_pem: private
                .to_pkcs8_pem(LineEnding::LF)
                .expect("encode RSA test key")
                .to_string(),
            n: URL_SAFE_NO_PAD.encode(public.n().to_bytes_be()),
            e: URL_SAFE_NO_PAD.encode(public.e().to_bytes_be()),
        }
    }

    fn jwk(&self) -> Value {
        json!({
            "kty": "RSA",
            "use": "sig",
            "alg": "RS256",
            "kid": self.kid,
            "n": self.n,
            "e": self.e,
        })
    }
}

struct TestKeys {
    first: TestKey,
    rotated: TestKey,
}

fn keys() -> &'static TestKeys {
    static KEYS: OnceLock<TestKeys> = OnceLock::new();
    KEYS.get_or_init(|| TestKeys {
        first: TestKey::generate("key-1"),
        rotated: TestKey::generate("key-2"),
    })
}

#[derive(Clone)]
enum FetchOutcome {
    Body(Vec<u8>),
    BodyWithMaxAge(Vec<u8>, Duration),
    Delayed(Duration, Vec<u8>),
    Gated(Arc<FetchGate>, Vec<u8>),
    Error,
    Panic,
}

struct FetchGate {
    started: AtomicBool,
    started_notify: Notify,
    release: Semaphore,
}

impl FetchGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            started: AtomicBool::new(false),
            started_notify: Notify::new(),
            release: Semaphore::new(0),
        })
    }

    async fn wait_started(&self) {
        while !self.started.load(Ordering::SeqCst) {
            self.started_notify.notified().await;
        }
    }

    fn release(&self) {
        self.release.add_permits(1);
    }
}

struct MockFetcher {
    outcomes: Mutex<VecDeque<FetchOutcome>>,
    calls: AtomicUsize,
}

impl MockFetcher {
    fn new(outcomes: impl IntoIterator<Item = FetchOutcome>) -> Arc<Self> {
        Arc::new(Self {
            outcomes: Mutex::new(outcomes.into_iter().collect()),
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl fmt::Debug for MockFetcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MockFetcher")
            .field("calls", &self.calls())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl JwksFetcher for MockFetcher {
    async fn fetch(&self, url: &Url) -> Result<JwksResponse, JwksFetchError> {
        assert_eq!(
            url.as_str(),
            "https://cellar.cloudflareaccess.com/cdn-cgi/access/certs"
        );
        self.calls.fetch_add(1, Ordering::SeqCst);
        let outcome = self
            .outcomes
            .lock()
            .expect("fetch outcome lock")
            .pop_front()
            .unwrap_or(FetchOutcome::Error);
        match outcome {
            FetchOutcome::Body(body) => Ok(JwksResponse::new(
                200,
                Cursor::new(body),
                Some(Duration::from_secs(600)),
            )),
            FetchOutcome::BodyWithMaxAge(body, max_age) => {
                Ok(JwksResponse::new(200, Cursor::new(body), Some(max_age)))
            }
            FetchOutcome::Delayed(delay, body) => {
                tokio::time::sleep(delay).await;
                Ok(JwksResponse::new(
                    200,
                    Cursor::new(body),
                    Some(Duration::from_secs(600)),
                ))
            }
            FetchOutcome::Gated(gate, body) => {
                gate.started.store(true, Ordering::SeqCst);
                gate.started_notify.notify_waiters();
                gate.release.acquire().await.expect("fetch gate").forget();
                Ok(JwksResponse::new(
                    200,
                    Cursor::new(body),
                    Some(Duration::from_secs(600)),
                ))
            }
            FetchOutcome::Error => Err(JwksFetchError::unavailable()),
            FetchOutcome::Panic => panic!("intentional fetcher panic"),
        }
    }
}

struct StalledBody;

impl AsyncRead for StalledBody {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Pending
    }
}

fn jwks(keys: &[&TestKey]) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "keys": keys.iter().map(|key| key.jwk()).collect::<Vec<_>>()
    }))
    .expect("serialize JWKS")
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time")
        .as_secs()
        .try_into()
        .expect("Unix timestamp")
}

fn claims() -> Value {
    let now = now();
    json!({
        "iss": ISSUER,
        "aud": [AUDIENCE],
        "sub": OWNER_SUBJECT,
        "email": OWNER_EMAIL,
        "exp": now + 300,
        "nbf": now - 30,
        "iat": now - 30,
        "type": "app",
    })
}

fn token_with(key: &TestKey, claims: &Value) -> String {
    token_with_header(key, claims, Algorithm::RS256, Some("JWT"), Some(key.kid))
}

fn token_with_header(
    key: &TestKey,
    claims: &Value,
    algorithm: Algorithm,
    typ: Option<&str>,
    kid: Option<&str>,
) -> String {
    let mut header = Header::new(algorithm);
    header.typ = typ.map(str::to_owned);
    header.kid = kid.map(str::to_owned);
    encode(
        &header,
        claims,
        &EncodingKey::from_rsa_pem(key.private_pem.as_bytes()).expect("RSA encoding key"),
    )
    .expect("encode JWT")
}

fn raw_signed_token(header_json: &str, claims_json: &str, key: &TestKey) -> String {
    let header = URL_SAFE_NO_PAD.encode(header_json);
    let claims = URL_SAFE_NO_PAD.encode(claims_json);
    let message = format!("{header}.{claims}");
    let encoding_key =
        EncodingKey::from_rsa_pem(key.private_pem.as_bytes()).expect("RSA encoding key");
    let signature = sign(message.as_bytes(), &encoding_key, Algorithm::RS256).expect("sign JWT");
    format!("{message}.{signature}")
}

fn hs256_token(claims: &Value) -> String {
    let mut header = Header::new(Algorithm::HS256);
    header.typ = Some("JWT".into());
    header.kid = Some(keys().first.kid.into());
    encode(
        &header,
        claims,
        &EncodingKey::from_secret(b"not-an-rsa-key"),
    )
    .expect("encode HS JWT")
}

fn config() -> AccessValidatorConfig {
    AccessValidatorConfig::new(
        ISSUER,
        AUDIENCE,
        "https://cellar.cloudflareaccess.com/cdn-cgi/access/certs",
    )
    .expect("valid validator config")
    .with_clock_skew(Duration::from_secs(5))
    .with_fetch_timeout(Duration::from_millis(100))
    .with_cache_ttl(Duration::from_secs(300))
    .with_min_refresh_interval(Duration::ZERO)
}

fn multi_audience_config() -> AccessValidatorConfig {
    AccessValidatorConfig::new_with_audiences(
        ISSUER,
        ["first-audience", "later-audience"],
        "https://cellar.cloudflareaccess.com/cdn-cgi/access/certs",
    )
    .expect("valid multi-audience validator config")
    .with_clock_skew(Duration::from_secs(5))
    .with_fetch_timeout(Duration::from_millis(100))
    .with_cache_ttl(Duration::from_secs(300))
    .with_min_refresh_interval(Duration::ZERO)
}

fn validator(fetcher: Arc<MockFetcher>) -> AccessValidator {
    AccessValidator::new(config(), fetcher)
}

fn enrolled() -> OwnerMode<'static> {
    OwnerMode::Enrolled {
        owner_subject: OWNER_SUBJECT,
    }
}

async fn assert_code(
    validator: &AccessValidator,
    token: &str,
    mode: OwnerMode<'_>,
    code: &'static str,
) {
    let error = validator
        .validate(token, mode)
        .await
        .expect_err("token must be rejected");
    assert_eq!(error.code(), code);
    assert_eq!(error.to_string(), code);
    assert_eq!(format!("{error:?}"), code);
    assert!(!error.to_string().contains(token));
}

#[tokio::test]
async fn accepts_valid_owner_and_preserves_claims() {
    let fetcher = MockFetcher::new([FetchOutcome::Body(jwks(&[&keys().first]))]);
    let expected = claims();
    let claims = validator(fetcher)
        .validate(&token_with(&keys().first, &expected), enrolled())
        .await
        .expect("valid owner JWT");

    assert_eq!(
        claims,
        AccessClaims {
            iss: ISSUER.into(),
            aud: vec![AUDIENCE.into()],
            sub: OWNER_SUBJECT.into(),
            email: Some(OWNER_EMAIL.into()),
            exp: expected["exp"].as_i64().expect("exp"),
            nbf: expected["nbf"].as_i64().expect("nbf"),
            iat: expected["iat"].as_i64().expect("iat"),
            r#type: "app".into(),
        }
    );
}

#[tokio::test]
async fn validates_exact_issuer_audience_and_app_type() {
    let fetcher = MockFetcher::new([FetchOutcome::Body(jwks(&[&keys().first]))]);
    let validator = validator(fetcher);

    let mut wrong_issuer = claims();
    wrong_issuer["iss"] = json!("https://other.cloudflareaccess.com");
    assert_code(
        &validator,
        &token_with(&keys().first, &wrong_issuer),
        enrolled(),
        "invalid_issuer",
    )
    .await;

    let mut wrong_audience = claims();
    wrong_audience["aud"] = json!(["other-audience"]);
    assert_code(
        &validator,
        &token_with(&keys().first, &wrong_audience),
        enrolled(),
        "invalid_audience",
    )
    .await;

    let mut service = claims();
    service["type"] = json!("service_token");
    assert_code(
        &validator,
        &token_with(&keys().first, &service),
        enrolled(),
        "service_token_forbidden",
    )
    .await;

    let mut wrong_type = claims();
    wrong_type["type"] = json!("user");
    assert_code(
        &validator,
        &token_with(&keys().first, &wrong_type),
        enrolled(),
        "invalid_token_type",
    )
    .await;
}

#[tokio::test]
async fn accepts_first_or_later_configured_audience_and_rejects_no_match() {
    let fetcher = MockFetcher::new([FetchOutcome::Body(jwks(&[&keys().first]))]);
    let validator = AccessValidator::new(multi_audience_config(), fetcher.clone());

    for audience in ["first-audience", "later-audience"] {
        let mut matching = claims();
        matching["aud"] = json!([audience]);
        validator
            .validate(&token_with(&keys().first, &matching), enrolled())
            .await
            .expect("one configured audience matches");
    }

    let mut no_match = claims();
    no_match["aud"] = json!(["unconfigured-audience"]);
    assert_code(
        &validator,
        &token_with(&keys().first, &no_match),
        enrolled(),
        "invalid_audience",
    )
    .await;
    assert_eq!(fetcher.calls(), 1);
}

#[test]
fn multi_audience_config_rejects_empty_duplicate_and_over_limit_without_leaking_values() {
    let url = "https://cellar.cloudflareaccess.com/cdn-cgi/access/certs";
    for audiences in [
        Vec::<String>::new(),
        vec![" secret-audience".into()],
        vec!["secret-audience".into(), "secret-audience".into()],
        (0..=MAX_AUD_TAGS)
            .map(|index| format!("secret-audience-{index}"))
            .collect(),
    ] {
        let error = AccessValidatorConfig::new_with_audiences(ISSUER, audiences, url)
            .expect_err("invalid audience collection");
        assert_eq!(error.code(), "invalid_auth_config");
        assert!(!format!("{error:?}").contains("secret-audience"));
    }

    let config = multi_audience_config();
    let debug = format!("{config:?}");
    assert!(!debug.contains("first-audience"));
    assert!(!debug.contains("later-audience"));
}

#[test]
fn multi_audience_config_bounds_collection_before_allocating() {
    let mut emitted = 0_usize;
    let unbounded = std::iter::from_fn(move || {
        assert!(
            emitted <= MAX_AUD_TAGS,
            "constructor read beyond the bounded rejection threshold"
        );
        let audience = format!("audience-{emitted}");
        emitted += 1;
        Some(audience)
    });

    let error = AccessValidatorConfig::new_with_audiences(
        ISSUER,
        unbounded,
        "https://cellar.cloudflareaccess.com/cdn-cgi/access/certs",
    )
    .expect_err("unbounded audience iterator");
    assert_eq!(error.code(), "invalid_auth_config");
}

#[tokio::test]
async fn validates_expiration_not_before_and_issued_at_with_skew() {
    let fetcher = MockFetcher::new([FetchOutcome::Body(jwks(&[&keys().first]))]);
    let validator = validator(fetcher);

    let mut expired = claims();
    expired["exp"] = json!(now() - 60);
    assert_code(
        &validator,
        &token_with(&keys().first, &expired),
        enrolled(),
        "token_expired",
    )
    .await;

    let mut not_yet_valid = claims();
    not_yet_valid["nbf"] = json!(now() + 60);
    assert_code(
        &validator,
        &token_with(&keys().first, &not_yet_valid),
        enrolled(),
        "token_not_yet_valid",
    )
    .await;

    let mut future_iat = claims();
    future_iat["iat"] = json!(now() + 60);
    assert_code(
        &validator,
        &token_with(&keys().first, &future_iat),
        enrolled(),
        "token_issued_in_future",
    )
    .await;
}

#[tokio::test]
async fn validates_subject_and_mode_specific_owner_identity() {
    let fetcher = MockFetcher::new([FetchOutcome::Body(jwks(&[&keys().first]))]);
    let validator = validator(fetcher);

    let mut empty_subject = claims();
    empty_subject["sub"] = json!("");
    assert_code(
        &validator,
        &token_with(&keys().first, &empty_subject),
        enrolled(),
        "invalid_subject",
    )
    .await;

    assert_code(
        &validator,
        &token_with(&keys().first, &claims()),
        OwnerMode::Enrolled {
            owner_subject: "different-subject",
        },
        "owner_subject_mismatch",
    )
    .await;

    assert_code(
        &validator,
        &token_with(&keys().first, &claims()),
        OwnerMode::Unenrolled {
            bootstrap_email: "different@example.com",
        },
        "bootstrap_email_mismatch",
    )
    .await;

    let accepted = validator
        .validate(
            &token_with(&keys().first, &claims()),
            OwnerMode::Unenrolled {
                bootstrap_email: OWNER_EMAIL,
            },
        )
        .await
        .expect("bootstrap email owner");
    assert_eq!(accepted.sub, OWNER_SUBJECT);
}

#[tokio::test]
async fn missing_required_claims_and_scalar_audience_fail_closed() {
    for field in ["iss", "aud", "sub", "exp", "nbf", "iat", "type"] {
        let fetcher = MockFetcher::new([FetchOutcome::Body(jwks(&[&keys().first]))]);
        let validator = validator(fetcher);
        let mut value = claims();
        value.as_object_mut().expect("claim object").remove(field);
        assert_code(
            &validator,
            &token_with(&keys().first, &value),
            enrolled(),
            "malformed_claims",
        )
        .await;
    }

    let fetcher = MockFetcher::new([FetchOutcome::Body(jwks(&[&keys().first]))]);
    let validator = validator(fetcher);
    let mut scalar_aud = claims();
    scalar_aud["aud"] = json!(AUDIENCE);
    assert_code(
        &validator,
        &token_with(&keys().first, &scalar_aud),
        enrolled(),
        "malformed_claims",
    )
    .await;
}

#[tokio::test]
async fn rejects_algorithm_typ_and_kid_confusion_before_fetch() {
    let fetcher = MockFetcher::new([]);
    let validator = validator(fetcher.clone());

    assert_code(
        &validator,
        &hs256_token(&claims()),
        enrolled(),
        "invalid_algorithm",
    )
    .await;

    let wrong_typ = token_with_header(
        &keys().first,
        &claims(),
        Algorithm::RS256,
        Some("at+jwt"),
        Some(keys().first.kid),
    );
    assert_code(&validator, &wrong_typ, enrolled(), "invalid_typ").await;

    let missing_kid = token_with_header(
        &keys().first,
        &claims(),
        Algorithm::RS256,
        Some("JWT"),
        None,
    );
    assert_code(&validator, &missing_kid, enrolled(), "invalid_kid").await;

    let empty_kid = token_with_header(
        &keys().first,
        &claims(),
        Algorithm::RS256,
        Some("JWT"),
        Some(""),
    );
    assert_code(&validator, &empty_kid, enrolled(), "invalid_kid").await;

    let oversized_kid = "k".repeat(513);
    let oversized_kid = token_with_header(
        &keys().first,
        &claims(),
        Algorithm::RS256,
        Some("JWT"),
        Some(&oversized_kid),
    );
    assert_code(&validator, &oversized_kid, enrolled(), "invalid_kid").await;

    assert_eq!(fetcher.calls(), 0);
}

#[tokio::test]
async fn rejects_critical_and_duplicate_jose_header_members_before_fetch() {
    let fetcher = MockFetcher::new([]);
    let validator = validator(fetcher.clone());

    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(keys().first.kid.into());
    header.crit = Some(vec!["cellar-extension".into()]);
    header
        .extras
        .insert("cellar-extension".into(), "required".into());
    let critical = encode(
        &header,
        &claims(),
        &EncodingKey::from_rsa_pem(keys().first.private_pem.as_bytes()).expect("RSA encoding key"),
    )
    .expect("critical JWT");
    assert_code(
        &validator,
        &critical,
        enrolled(),
        "unsupported_critical_header",
    )
    .await;

    let now = now();
    let duplicate_header = raw_signed_token(
        r#"{"alg":"RS256","alg":"RS256","typ":"JWT","kid":"key-1"}"#,
        &format!(
            r#"{{"iss":"{ISSUER}","aud":["{AUDIENCE}"],"sub":"{OWNER_SUBJECT}","email":"{OWNER_EMAIL}","exp":{},"nbf":{},"iat":{},"type":"app"}}"#,
            now + 300,
            now - 30,
            now - 30
        ),
        &keys().first,
    );
    assert_code(&validator, &duplicate_header, enrolled(), "malformed_token").await;
    assert_eq!(fetcher.calls(), 0);
}

#[tokio::test]
async fn rejects_duplicate_claim_members_after_signature_verification() {
    let now = now();
    let duplicate_claims = raw_signed_token(
        r#"{"alg":"RS256","typ":"JWT","kid":"key-1"}"#,
        &format!(
            r#"{{"iss":"{ISSUER}","aud":["{AUDIENCE}"],"sub":"{OWNER_SUBJECT}","sub":"other","email":"{OWNER_EMAIL}","exp":{},"nbf":{},"iat":{},"type":"app"}}"#,
            now + 300,
            now - 30,
            now - 30
        ),
        &keys().first,
    );
    let fetcher = MockFetcher::new([FetchOutcome::Body(jwks(&[&keys().first]))]);
    assert_code(
        &validator(fetcher),
        &duplicate_claims,
        enrolled(),
        "malformed_claims",
    )
    .await;
}

#[tokio::test]
async fn rejects_forged_signature() {
    let fetcher = MockFetcher::new([FetchOutcome::Body(jwks(&[&keys().first]))]);
    let forged = token_with_header(
        &keys().rotated,
        &claims(),
        Algorithm::RS256,
        Some("JWT"),
        Some(keys().first.kid),
    );
    assert_code(
        &validator(fetcher),
        &forged,
        enrolled(),
        "invalid_signature",
    )
    .await;
}

#[test]
fn header_helper_accepts_exactly_one_bounded_utf8_value() {
    assert_eq!(
        select_access_jwt_header([b"abc".as_slice()], 3).expect("one header"),
        "abc"
    );
    assert_eq!(
        select_access_jwt_header(std::iter::empty::<&[u8]>(), 100)
            .expect_err("missing")
            .code(),
        "missing_access_token"
    );
    assert_eq!(
        select_access_jwt_header([b"a".as_slice(), b"b".as_slice()], 100)
            .expect_err("duplicate")
            .code(),
        "duplicate_access_token"
    );
    assert_eq!(
        select_access_jwt_header([&[0xff][..]], 100)
            .expect_err("non UTF-8")
            .code(),
        "invalid_access_token_header"
    );
    assert_eq!(
        select_access_jwt_header([b"abcd".as_slice()], 3)
            .expect_err("oversized")
            .code(),
        "access_token_too_large"
    );
}

#[tokio::test]
async fn rejects_malformed_and_oversized_encoded_tokens_before_fetch() {
    let fetcher = MockFetcher::new([]);
    let validator = validator(fetcher.clone());
    assert_code(&validator, "not-a-jwt", enrolled(), "malformed_token").await;

    let validator = AccessValidator::new(config().with_max_token_len(16), fetcher.clone());
    assert_code(
        &validator,
        "0123456789abcdefg",
        enrolled(),
        "access_token_too_large",
    )
    .await;
    assert_eq!(fetcher.calls(), 0);
}

#[tokio::test(start_paused = true)]
async fn unknown_kid_refreshes_once_and_accepts_rotated_key() {
    let fetcher = MockFetcher::new([
        FetchOutcome::Body(jwks(&[&keys().first])),
        FetchOutcome::Body(jwks(&[&keys().rotated])),
    ]);
    let validator = validator(fetcher.clone());

    validator
        .validate(&token_with(&keys().first, &claims()), enrolled())
        .await
        .expect("initial key");
    tokio::time::advance(Duration::from_millis(2)).await;
    validator
        .validate(&token_with(&keys().rotated, &claims()), enrolled())
        .await
        .expect("rotated key after refresh");
    assert_eq!(fetcher.calls(), 2);
}

#[tokio::test]
async fn unsupported_jwk_key_type_use_and_alg_are_never_cached() {
    let invalid = serde_json::to_vec(&json!({
        "keys": [
            {"kty":"EC", "use":"sig", "alg":"RS256", "kid":"key-1", "n":keys().first.n, "e":keys().first.e},
            {"kty":"RSA", "use":"enc", "alg":"RS256", "kid":"invalid-use", "n":keys().first.n, "e":keys().first.e},
            {"kty":"RSA", "use":"sig", "alg":"RS512", "kid":"invalid-alg", "n":keys().first.n, "e":keys().first.e}
        ]
    }))
    .expect("invalid JWKS");
    let fetcher = MockFetcher::new([FetchOutcome::Body(invalid)]);
    assert_code(
        &validator(fetcher),
        &token_with(&keys().first, &claims()),
        enrolled(),
        "jwks_malformed",
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn fully_filtered_jwks_is_not_cached_as_fresh_empty_cache() {
    let unsupported = serde_json::to_vec(&json!({
        "keys": [{"kty":"EC", "use":"sig", "alg":"ES256", "kid":"other"}]
    }))
    .expect("unsupported JWKS");
    let fetcher = MockFetcher::new([
        FetchOutcome::Body(unsupported),
        FetchOutcome::Body(jwks(&[&keys().first])),
    ]);
    let validator = AccessValidator::new(
        config()
            .with_min_refresh_interval(Duration::from_secs(300))
            .with_failure_backoff(Duration::from_millis(30)),
        fetcher.clone(),
    );
    let token = token_with(&keys().first, &claims());

    assert_code(&validator, &token, enrolled(), "jwks_malformed").await;
    tokio::time::advance(Duration::from_millis(31)).await;
    validator
        .validate(&token, enrolled())
        .await
        .expect("empty cache retries after bounded failure backoff");
    assert_eq!(fetcher.calls(), 2);
}

#[tokio::test]
async fn duplicate_kid_is_rejected_before_jwk_compatibility_filtering() {
    let duplicate = serde_json::to_vec(&json!({
        "keys": [
            keys().first.jwk(),
            {"kty":"EC", "use":"sig", "alg":"ES256", "kid":"key-1"}
        ]
    }))
    .expect("duplicate-kid JWKS");
    let fetcher = MockFetcher::new([FetchOutcome::Body(duplicate)]);
    assert_code(
        &validator(fetcher),
        &token_with(&keys().first, &claims()),
        enrolled(),
        "jwks_malformed",
    )
    .await;
}

#[tokio::test]
async fn duplicate_jwk_json_member_is_rejected() {
    let duplicate_member = format!(
        r#"{{"keys":[{{"kty":"RSA","use":"sig","alg":"RS256","kid":"key-1","kid":"key-2","n":"{}","e":"{}"}}]}}"#,
        keys().first.n,
        keys().first.e
    )
    .into_bytes();
    let fetcher = MockFetcher::new([FetchOutcome::Body(duplicate_member)]);
    assert_code(
        &validator(fetcher),
        &token_with(&keys().first, &claims()),
        enrolled(),
        "jwks_malformed",
    )
    .await;
}

#[tokio::test]
async fn oversized_malformed_and_too_many_key_sets_fail_closed() {
    let oversized = MockFetcher::new([FetchOutcome::Body(vec![b'x'; 257])]);
    let oversized_validator =
        AccessValidator::new(config().with_max_jwks_bytes(256), oversized.clone());
    assert_code(
        &oversized_validator,
        &token_with(&keys().first, &claims()),
        enrolled(),
        "jwks_response_too_large",
    )
    .await;

    let malformed = MockFetcher::new([FetchOutcome::Body(b"{not-json".to_vec())]);
    assert_code(
        &validator(malformed),
        &token_with(&keys().first, &claims()),
        enrolled(),
        "jwks_malformed",
    )
    .await;

    let too_many_body = serde_json::to_vec(&json!({
        "keys": [keys().first.jwk(), keys().rotated.jwk()]
    }))
    .expect("large JWKS");
    let too_many = MockFetcher::new([FetchOutcome::Body(too_many_body)]);
    let validator = AccessValidator::new(config().with_max_jwks_keys(1), too_many);
    assert_code(
        &validator,
        &token_with(&keys().first, &claims()),
        enrolled(),
        "jwks_too_many_keys",
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn timeout_covers_fetch_and_fails_closed() {
    let fetcher = MockFetcher::new([FetchOutcome::Delayed(
        Duration::from_millis(100),
        jwks(&[&keys().first]),
    )]);
    let validator = AccessValidator::new(
        config().with_fetch_timeout(Duration::from_millis(10)),
        fetcher,
    );
    assert_code(
        &validator,
        &token_with(&keys().first, &claims()),
        enrolled(),
        "jwks_timeout",
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn timeout_also_covers_streaming_response_body() {
    struct StalledFetcher;

    #[async_trait]
    impl JwksFetcher for StalledFetcher {
        async fn fetch(&self, _url: &Url) -> Result<JwksResponse, JwksFetchError> {
            Ok(JwksResponse::new(200, StalledBody, None))
        }
    }

    let validator = AccessValidator::new(
        config().with_fetch_timeout(Duration::from_millis(10)),
        Arc::new(StalledFetcher),
    );
    assert_code(
        &validator,
        &token_with(&keys().first, &claims()),
        enrolled(),
        "jwks_timeout",
    )
    .await;
}

#[tokio::test]
async fn oversized_jwk_fields_fail_closed() {
    let body = serde_json::to_vec(&json!({
        "keys": [{
            "kty": "RSA",
            "use": "sig",
            "alg": "RS256",
            "kid": "k".repeat(16 * 1024 + 1),
            "n": keys().first.n,
            "e": keys().first.e,
        }]
    }))
    .expect("oversized-field JWKS");
    let fetcher = MockFetcher::new([FetchOutcome::Body(body)]);
    assert_code(
        &validator(fetcher),
        &token_with(&keys().first, &claims()),
        enrolled(),
        "jwks_malformed",
    )
    .await;
}

#[tokio::test]
async fn concurrent_unknown_kid_refresh_is_single_flight() {
    let gate = FetchGate::new();
    let fetcher = MockFetcher::new([FetchOutcome::Gated(gate.clone(), jwks(&[&keys().first]))]);
    let validator = Arc::new(validator(fetcher.clone()));
    let token = Arc::new(token_with(&keys().first, &claims()));

    let mut tasks = Vec::new();
    for _ in 0..24 {
        let validator = validator.clone();
        let token = token.clone();
        tasks.push(tokio::spawn(async move {
            validator.validate(&token, enrolled()).await
        }));
    }
    gate.wait_started().await;
    assert_eq!(fetcher.calls(), 1);
    gate.release();
    for task in tasks {
        task.await
            .expect("validation task")
            .expect("single-flight validation");
    }
    assert_eq!(fetcher.calls(), 1);
}

#[tokio::test]
async fn multiple_audiences_share_one_initial_and_unknown_kid_refresh_under_concurrency() {
    let initial_gate = FetchGate::new();
    let rotated_gate = FetchGate::new();
    let fetcher = MockFetcher::new([
        FetchOutcome::Gated(initial_gate.clone(), jwks(&[&keys().first])),
        FetchOutcome::Gated(rotated_gate.clone(), jwks(&[&keys().rotated])),
    ]);
    let validator = Arc::new(AccessValidator::new(
        multi_audience_config(),
        fetcher.clone(),
    ));

    async fn validate_concurrently(
        validator: &Arc<AccessValidator>,
        key: &TestKey,
        gate: &Arc<FetchGate>,
        expected_calls: usize,
        fetcher: &Arc<MockFetcher>,
    ) {
        let mut tasks = Vec::new();
        for index in 0..24 {
            let mut value = claims();
            value["aud"] = json!([if index % 2 == 0 {
                "first-audience"
            } else {
                "later-audience"
            }]);
            let token = token_with(key, &value);
            let validator = validator.clone();
            tasks.push(tokio::spawn(async move {
                validator.validate(&token, enrolled()).await
            }));
        }
        gate.wait_started().await;
        assert_eq!(fetcher.calls(), expected_calls);
        gate.release();
        for task in tasks {
            task.await
                .expect("validation task")
                .expect("shared multi-audience validation");
        }
        assert_eq!(fetcher.calls(), expected_calls);
    }

    validate_concurrently(&validator, &keys().first, &initial_gate, 1, &fetcher).await;
    validate_concurrently(&validator, &keys().rotated, &rotated_gate, 2, &fetcher).await;
}

#[tokio::test]
async fn cancelled_waiter_does_not_cancel_or_wedge_shared_refresh() {
    let gate = FetchGate::new();
    let fetcher = MockFetcher::new([FetchOutcome::Gated(gate.clone(), jwks(&[&keys().first]))]);
    let validator = Arc::new(validator(fetcher.clone()));
    let token = token_with(&keys().first, &claims());

    let task = {
        let validator = validator.clone();
        let token = token.clone();
        tokio::spawn(async move { validator.validate(&token, enrolled()).await })
    };
    gate.wait_started().await;
    task.abort();
    task.await.expect_err("validation waiter cancelled");
    gate.release();

    tokio::time::timeout(
        Duration::from_secs(1),
        validator.validate(&token, enrolled()),
    )
    .await
    .expect("refresh completion timeout")
    .expect("detached refresh completed after waiter cancellation");
    assert_eq!(fetcher.calls(), 1);
}

#[tokio::test(start_paused = true)]
async fn unknown_kid_refresh_is_rate_limited_between_attempts() {
    let fetcher = MockFetcher::new([
        FetchOutcome::Body(jwks(&[&keys().first])),
        FetchOutcome::Body(jwks(&[&keys().rotated])),
    ]);
    let validator = AccessValidator::new(
        config().with_min_refresh_interval(Duration::from_millis(30)),
        fetcher.clone(),
    );
    validator
        .validate(&token_with(&keys().first, &claims()), enrolled())
        .await
        .expect("initial key");

    assert_code(
        &validator,
        &token_with(&keys().rotated, &claims()),
        enrolled(),
        "jwks_refresh_rate_limited",
    )
    .await;
    assert_eq!(fetcher.calls(), 1);

    tokio::time::advance(Duration::from_millis(35)).await;
    validator
        .validate(&token_with(&keys().rotated, &claims()), enrolled())
        .await
        .expect("refresh allowed after interval");
    assert_eq!(fetcher.calls(), 2);
}

#[tokio::test(start_paused = true)]
async fn failed_or_panicked_refresh_does_not_wedge_later_retry() {
    for first in [FetchOutcome::Error, FetchOutcome::Panic] {
        let fetcher = MockFetcher::new([first, FetchOutcome::Body(jwks(&[&keys().first]))]);
        let validator = AccessValidator::new(
            config().with_failure_backoff(Duration::from_millis(10)),
            fetcher.clone(),
        );
        let token = token_with(&keys().first, &claims());

        let first_error = validator
            .validate(&token, enrolled())
            .await
            .expect_err("first refresh fails");
        assert_eq!(first_error.code(), "jwks_refresh_failed");

        tokio::time::advance(Duration::from_millis(15)).await;
        validator
            .validate(&token, enrolled())
            .await
            .expect("later retry succeeds");
        assert_eq!(fetcher.calls(), 2);
    }
}

#[tokio::test(start_paused = true)]
async fn failed_mandatory_refresh_uses_bounded_backoff_without_fetch_storm() {
    let fetcher = MockFetcher::new([
        FetchOutcome::Error,
        FetchOutcome::Body(jwks(&[&keys().first])),
    ]);
    let validator = AccessValidator::new(
        config().with_failure_backoff(Duration::from_millis(30)),
        fetcher.clone(),
    );
    let token = token_with(&keys().first, &claims());

    assert_code(&validator, &token, enrolled(), "jwks_refresh_failed").await;
    assert_code(&validator, &token, enrolled(), "jwks_refresh_failed").await;
    assert_eq!(fetcher.calls(), 1);

    tokio::time::advance(Duration::from_millis(31)).await;
    validator
        .validate(&token, enrolled())
        .await
        .expect("mandatory refresh retries after bounded failure backoff");
    assert_eq!(fetcher.calls(), 2);
}

#[tokio::test(start_paused = true)]
async fn stale_known_key_requires_successful_refresh() {
    let fetcher = MockFetcher::new([
        FetchOutcome::Body(jwks(&[&keys().first])),
        FetchOutcome::Error,
    ]);
    let validator = AccessValidator::new(
        config()
            .with_cache_ttl(Duration::from_millis(5))
            .with_min_refresh_interval(Duration::ZERO),
        fetcher,
    );
    let token = token_with(&keys().first, &claims());

    validator
        .validate(&token, enrolled())
        .await
        .expect("fresh cached key");
    tokio::time::advance(Duration::from_millis(10)).await;
    assert_code(&validator, &token, enrolled(), "jwks_refresh_failed").await;
}

#[tokio::test(start_paused = true)]
async fn mandatory_expired_cache_refresh_ignores_unknown_kid_interval() {
    let fetcher = MockFetcher::new([
        FetchOutcome::BodyWithMaxAge(jwks(&[&keys().first]), Duration::from_secs(1)),
        FetchOutcome::BodyWithMaxAge(jwks(&[&keys().first]), Duration::from_secs(1)),
    ]);
    let validator = AccessValidator::new(
        config().with_min_refresh_interval(Duration::from_secs(300)),
        fetcher.clone(),
    );
    let token = token_with(&keys().first, &claims());

    validator
        .validate(&token, enrolled())
        .await
        .expect("initial mandatory refresh");
    tokio::time::advance(Duration::from_secs(2)).await;
    validator
        .validate(&token, enrolled())
        .await
        .expect("expired cache performs mandatory refresh");
    assert_eq!(fetcher.calls(), 2);
}

#[tokio::test(start_paused = true)]
async fn concurrent_refresh_installing_requested_kid_is_observed_before_rate_limit() {
    let gate = FetchGate::new();
    let fetcher = MockFetcher::new([
        FetchOutcome::Body(jwks(&[&keys().first])),
        FetchOutcome::Gated(gate.clone(), jwks(&[&keys().rotated])),
    ]);
    let validator = Arc::new(AccessValidator::new(
        config()
            .with_cache_ttl(Duration::from_secs(600))
            .with_min_refresh_interval(Duration::from_secs(300)),
        fetcher.clone(),
    ));
    validator
        .validate(&token_with(&keys().first, &claims()), enrolled())
        .await
        .expect("prime cache");
    tokio::time::advance(Duration::from_secs(301)).await;

    let rotated = Arc::new(token_with(&keys().rotated, &claims()));
    let refresher = {
        let validator = validator.clone();
        let rotated = rotated.clone();
        tokio::spawn(async move { validator.validate(&rotated, enrolled()).await })
    };
    gate.wait_started().await;
    let racing_waiter = validator.validate(&rotated, enrolled());
    tokio::pin!(racing_waiter);
    tokio::select! {
        biased;
        result = &mut racing_waiter => panic!("racing waiter completed before refresh: {result:?}"),
        () = tokio::task::yield_now() => {}
    }
    gate.release();

    refresher
        .await
        .expect("refresher task")
        .expect("refresher validation");
    racing_waiter
        .await
        .expect("racing waiter observes installed key");
    assert_eq!(fetcher.calls(), 2);
}

#[test]
fn configuration_requires_https_and_nonempty_identity_values() {
    for (issuer, audience, jwks_url) in [
        (
            "http://cellar.cloudflareaccess.com",
            AUDIENCE,
            "https://example.com/keys",
        ),
        (ISSUER, AUDIENCE, "http://example.com/keys"),
        (ISSUER, "", "https://example.com/keys"),
        ("", AUDIENCE, "https://example.com/keys"),
        (ISSUER, AUDIENCE, "https://example.com/cdn-cgi/access/certs"),
        (
            ISSUER,
            AUDIENCE,
            "https://cellar.cloudflareaccess.com/other",
        ),
    ] {
        let error = AccessValidatorConfig::new(issuer, audience, jwks_url)
            .expect_err("invalid secure configuration");
        assert_eq!(error.code(), "invalid_auth_config");
    }
}

#[tokio::test]
async fn canonicalizes_configured_issuer_but_requires_exact_canonical_claim() {
    let canonicalized = AccessValidatorConfig::new(
        "HTTPS://CELLAR.CLOUDFLAREACCESS.COM:443/",
        AUDIENCE,
        "HTTPS://CELLAR.CLOUDFLAREACCESS.COM:443/cdn-cgi/access/certs",
    )
    .expect("canonicalizable Cloudflare origin")
    .with_min_refresh_interval(Duration::from_millis(1));
    let fetcher = MockFetcher::new([FetchOutcome::Body(jwks(&[&keys().first]))]);
    let validator = AccessValidator::new(canonicalized, fetcher);
    validator
        .validate(&token_with(&keys().first, &claims()), enrolled())
        .await
        .expect("canonical Cloudflare iss");

    let mut noncanonical_claim = claims();
    noncanonical_claim["iss"] = json!("HTTPS://CELLAR.CLOUDFLAREACCESS.COM:443");
    assert_code(
        &validator,
        &token_with(&keys().first, &noncanonical_claim),
        enrolled(),
        "invalid_issuer",
    )
    .await;
}

#[test]
fn claim_and_owner_debug_output_redacts_identity_values() {
    let claims = AccessClaims {
        iss: ISSUER.into(),
        aud: vec![AUDIENCE.into()],
        sub: OWNER_SUBJECT.into(),
        email: Some(OWNER_EMAIL.into()),
        exp: 1,
        nbf: 1,
        iat: 1,
        r#type: "app".into(),
    };
    let claims_debug = format!("{claims:?}");
    for secret in [ISSUER, AUDIENCE, OWNER_SUBJECT, OWNER_EMAIL] {
        assert!(!claims_debug.contains(secret));
    }

    let mode = OwnerMode::Unenrolled {
        bootstrap_email: OWNER_EMAIL,
    };
    assert!(!format!("{mode:?}").contains(OWNER_EMAIL));
    let mode = OwnerMode::Enrolled {
        owner_subject: OWNER_SUBJECT,
    };
    assert!(!format!("{mode:?}").contains(OWNER_SUBJECT));
}
