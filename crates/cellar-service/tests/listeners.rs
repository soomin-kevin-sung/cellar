use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener as StdTcpListener};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use cellar_api::health::{Readiness, health_router};
use cellar_auth::{AccessClaims, OwnerMode};
use cellar_config::{CellarConfig, PersistedConfig, save_config};
use cellar_core::ReadinessBlocker;
use cellar_service::app::{
    ListenerConfig, OriginAuthenticator, Shutdown, bind_listeners, check_startup_gates,
    origin_router, origin_router_with_authenticator,
};
use cellar_service::logging::{
    ContextKey, JsonLogger, LogEvent, LogLevel, RotationPolicy, SanitizedContext, write_json,
};
use cellar_windows::service::{
    ACCEPT_PRESHUTDOWN, ACCEPT_STOP, RecoveryIntent, ServiceControl, accepted_controls,
    control_from_raw,
};
use http_body_util::BodyExt;
use tempfile::tempdir;
use time::OffsetDateTime;
use tower::ServiceExt;
use url::Url;

async fn response(method: Method, path: &str, app: axum::Router) -> (StatusCode, Vec<u8>) {
    let response = app
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, body.to_vec())
}

#[tokio::test]
async fn live_and_ready_are_bounded_and_reveal_only_stable_codes() {
    let readiness = Readiness::all_blocked();
    let app = health_router(readiness.clone());

    let (live_status, live_body) = response(Method::GET, "/health/live", app.clone()).await;
    assert_eq!(live_status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&live_body).unwrap(),
        serde_json::json!({"status":"live"})
    );

    readiness.clear(ReadinessBlocker::ConfigurationRequired);
    readiness.clear(ReadinessBlocker::OwnerEnrollmentRequired);
    readiness.clear(ReadinessBlocker::MigrationRequired);
    readiness.clear(ReadinessBlocker::RecoveryRequired);
    readiness.clear(ReadinessBlocker::StorageUnavailable);
    readiness.clear(ReadinessBlocker::ReconciliationRequired);
    readiness.clear(ReadinessBlocker::OriginTrustUpdateRequired);
    let (ready_status, ready_body) = response(Method::GET, "/health/ready", app).await;
    assert_eq!(ready_status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&ready_body).unwrap(),
        serde_json::json!({"status":"ready","blockers":[]})
    );
}

#[tokio::test]
async fn blocked_readiness_is_deduplicated_ordered_and_redacted() {
    let readiness = Readiness::new([
        ReadinessBlocker::StorageUnavailable,
        ReadinessBlocker::MigrationRequired,
        ReadinessBlocker::StorageUnavailable,
        ReadinessBlocker::ConfigurationRequired,
        ReadinessBlocker::OriginTrustUpdateRequired,
    ]);
    readiness.block(ReadinessBlocker::MigrationRequired);
    let app = health_router(readiness);

    let (status, body) = response(Method::GET, "/health/ready", app).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.len() <= 512);
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "status":"blocked",
            "blockers":[
                "configuration_required",
                "migration_required",
                "storage_unavailable",
                "origin_trust_update_required"
            ]
        })
    );
    let text = String::from_utf8(body).unwrap();
    assert!(!text.contains('\\'));
    assert!(!text.contains('@'));
}

#[tokio::test]
async fn health_and_origin_routes_are_strictly_separate() {
    let readiness = Readiness::new([]);
    let health = health_router(readiness);
    let origin = origin_router();

    assert_eq!(
        response(Method::GET, "/health/live", origin.clone())
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        response(Method::GET, "/api/v1/session", health.clone())
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        response(Method::POST, "/health/ready", health.clone())
            .await
            .0,
        StatusCode::METHOD_NOT_ALLOWED
    );
    let (missing_status, missing_body) = response(Method::GET, "/missing", health).await;
    assert_eq!(missing_status, StatusCode::NOT_FOUND);
    assert!(missing_body.is_empty());
}

struct AllowOwner;

#[async_trait::async_trait]
impl OriginAuthenticator for AllowOwner {
    async fn validate(
        &self,
        token: &str,
        _owner_mode: OwnerMode<'_>,
    ) -> Result<AccessClaims, cellar_auth::AuthError> {
        assert_eq!(token, "signed-access-token");
        Ok(AccessClaims {
            iss: "https://team.cloudflareaccess.com".into(),
            aud: vec!["audience".into()],
            sub: "owner-subject".into(),
            email: None,
            exp: i64::MAX,
            nbf: 0,
            iat: 0,
            r#type: "app".into(),
        })
    }

    fn max_token_len(&self) -> usize {
        1024
    }
}

fn enrolled_config(path: &std::path::Path) {
    save_config(
        path,
        &PersistedConfig {
            config: CellarConfig {
                external_origin: Url::parse("https://cellar.example.test").unwrap(),
                team_domain: Url::parse("https://team.cloudflareaccess.com").unwrap(),
                aud_tags: vec!["audience".into()],
                bootstrap_owner_email: None,
                owner_subject: Some("owner-subject".into()),
                storage_root: std::path::PathBuf::from(r"C:\cellar-storage"),
                origin_port: 8443,
                health_port: 8081,
            },
            bootstrap_claim: None,
        },
    )
    .unwrap();
}

fn access_request(method: Method, path: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header("cf-access-jwt-assertion", "signed-access-token")
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn production_origin_composes_authentication_and_session_without_health() {
    let directory = tempdir().unwrap();
    let config_path = directory.path().join("config.toml");
    enrolled_config(&config_path);
    let readiness = Readiness::all_blocked();
    let shutdown = Shutdown::new();
    let origin = origin_router_with_authenticator(
        &config_path,
        std::sync::Arc::new(AllowOwner),
        readiness.clone(),
        shutdown,
    );

    let session = origin
        .clone()
        .oneshot(access_request(Method::GET, "/api/v1/session"))
        .await
        .unwrap();
    assert_eq!(session.status(), StatusCode::OK);
    assert!(
        !readiness
            .blocker_codes()
            .contains(&"owner_enrollment_required")
    );
    assert_eq!(
        origin
            .oneshot(access_request(Method::GET, "/health/live"))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn production_origin_auth_failures_use_the_shared_request_envelope() {
    let directory = tempdir().unwrap();
    let config_path = directory.path().join("config.toml");
    enrolled_config(&config_path);
    let origin = origin_router_with_authenticator(
        &config_path,
        std::sync::Arc::new(AllowOwner),
        Readiness::new([]),
        Shutdown::new(),
    );
    let response = origin
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/v1/session")
                .header("x-request-id", "production-auth-request")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.headers().get("x-request-id").unwrap(),
        "production-auth-request"
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({
            "code": "missing_access_token",
            "message": "Authentication could not be completed.",
            "requestId": "production-auth-request",
            "details": {}
        })
    );
}

#[tokio::test]
async fn shutdown_rejects_new_unsafe_origin_requests_but_allows_safe_drain() {
    let directory = tempdir().unwrap();
    let config_path = directory.path().join("config.toml");
    enrolled_config(&config_path);
    let shutdown = Shutdown::new();
    let origin = origin_router_with_authenticator(
        &config_path,
        std::sync::Arc::new(AllowOwner),
        Readiness::new([]),
        shutdown.clone(),
    );
    shutdown.signal(ServiceControl::Stop);

    assert_eq!(
        origin
            .clone()
            .oneshot(access_request(Method::POST, "/api/v1/files"))
            .await
            .unwrap()
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    let anonymous = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/files")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        origin.clone().oneshot(anonymous).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert_ne!(
        origin
            .oneshot(access_request(Method::GET, "/api/v1/session"))
            .await
            .unwrap()
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn startup_gates_reject_only_unresolved_operation_evidence() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("cellar.db");
    let pool = cellar_db::open_pool(
        &database,
        cellar_db::FilenameCollation::windows_ordinal_ci_v1(str::cmp),
    )
    .await
    .unwrap();
    cellar_db::migrate(&pool).await.unwrap();
    let empty = check_startup_gates(&pool).await.unwrap();
    assert!(empty.recovery_complete);
    assert!(empty.reconciliation_complete);

    sqlx::query(
        "INSERT INTO operation
         (id, kind, state, payload_version, payload, created_at, updated_at)
         VALUES ('pending-op', 'test', 'pending', 1, '{}', 'now', 'now')",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        check_startup_gates(&pool).await.unwrap_err().code(),
        "startup_recovery_failed"
    );
    sqlx::query("DELETE FROM operation")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO project
         (id, name, status, version, created_at, updated_at)
         VALUES ('catalog-only', 'catalog only', 'active', 1, 'now', 'now')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let catalog = check_startup_gates(&pool).await.unwrap();
    assert!(catalog.recovery_complete);
    assert!(catalog.reconciliation_complete);
}

fn available_port() -> u16 {
    StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test]
async fn listeners_bind_separately_to_ipv4_loopback() {
    let origin_port = available_port();
    let mut health_port = available_port();
    while health_port == origin_port {
        health_port = available_port();
    }
    let listeners = bind_listeners(ListenerConfig {
        origin_port,
        health_port,
    })
    .await
    .unwrap();

    assert_eq!(
        listeners.origin_addr(),
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, origin_port).into()
    );
    assert_eq!(
        listeners.health_addr(),
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, health_port).into()
    );
    assert_ne!(listeners.origin_addr(), listeners.health_addr());
}

#[tokio::test]
async fn second_bind_failure_rolls_back_the_first_listener() {
    let occupied = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let health_port = occupied.local_addr().unwrap().port();
    let mut origin_port = available_port();
    while origin_port == health_port {
        origin_port = available_port();
    }

    let error = bind_listeners(ListenerConfig {
        origin_port,
        health_port,
    })
    .await
    .unwrap_err();
    assert_eq!(error.code(), "listener_bind_failed");
    StdTcpListener::bind((Ipv4Addr::LOCALHOST, origin_port))
        .expect("origin listener must be dropped transactionally");
}

#[tokio::test]
async fn shutdown_signal_disables_mutations_and_preserves_control_kind() {
    let shutdown = Shutdown::new();
    let waiter = shutdown.clone();
    let task = tokio::spawn(async move { waiter.wait().await });
    assert!(shutdown.accepting_mutations());

    shutdown.signal(ServiceControl::Preshutdown);

    assert_eq!(task.await.unwrap(), ServiceControl::Preshutdown);
    assert!(!shutdown.accepting_mutations());
    assert_eq!(shutdown.normal_stop_target(), Duration::from_secs(60));
    assert_eq!(shutdown.preshutdown_budget(), Duration::from_secs(180));
}

#[test]
fn scm_adapter_accepts_stop_and_preshutdown_with_bounded_recovery_intent() {
    assert_eq!(control_from_raw(1), Some(ServiceControl::Stop));
    assert_eq!(control_from_raw(15), Some(ServiceControl::Preshutdown));
    assert_eq!(control_from_raw(255), None);
    assert_eq!(accepted_controls(), ACCEPT_STOP | ACCEPT_PRESHUTDOWN);
    assert_eq!(
        RecoveryIntent::default().restart_delay,
        Duration::from_secs(30)
    );
    assert_eq!(RecoveryIntent::default().maximum_restarts, 3);
}

fn event() -> LogEvent {
    let mut context = SanitizedContext::new();
    context.insert(ContextKey::Component, "service").unwrap();
    LogEvent::at(
        OffsetDateTime::UNIX_EPOCH,
        LogLevel::Error,
        "startup_failed",
    )
    .with_request_id("req-7")
    .with_operation_id("op-9")
    .with_context(context)
}

#[test]
fn json_logs_have_the_fixed_schema_and_only_allow_sanitized_context() {
    let mut output = Vec::new();
    write_json(&mut output, &event()).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(value["timestamp"], "1970-01-01T00:00:00Z");
    assert_eq!(value["level"], "error");
    assert_eq!(value["event_code"], "startup_failed");
    assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(value["request_id"], "req-7");
    assert_eq!(value["operation_id"], "op-9");
    assert_eq!(value["context"], serde_json::json!({"component":"service"}));

    for secret in [
        "owner@example.test",
        r"C:\Users\owner\secret",
        "Bearer abc.def.ghi",
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJvd25lciJ9.signature",
        "session=cookie",
        "claim_code=bootstrap-value",
        "tunnel_secret=credential-value",
        "request_body=private-content",
    ] {
        let mut context = SanitizedContext::new();
        let error = context.insert(ContextKey::State, secret).unwrap_err();
        assert_eq!(error.code(), "unsafe_log_context");
        let rejected = LogEvent::at(
            OffsetDateTime::UNIX_EPOCH,
            LogLevel::Error,
            "rejected_context",
        )
        .with_context(context);
        let mut output = Vec::new();
        write_json(&mut output, &rejected).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains(secret));
        for forbidden_key in ["claim_code", "tunnel_secret", "credential", "request_body"] {
            assert!(!output.contains(forbidden_key));
        }
    }
    let mut context = SanitizedContext::new();
    context.insert(ContextKey::State, &"x".repeat(500)).unwrap();
    assert!(context.encoded_len() <= 256);
}

#[test]
fn service_entry_arguments_are_explicit_and_fail_closed() {
    use cellar_windows::service::{EntryMode, select_entry_mode};

    assert_eq!(
        select_entry_mode(Vec::<&str>::new()).unwrap(),
        EntryMode::Service
    );
    assert_eq!(
        select_entry_mode(["--service"]).unwrap(),
        EntryMode::Service
    );
    assert_eq!(
        select_entry_mode(["--console"]).unwrap(),
        EntryMode::Console
    );
    for arguments in [vec!["--unknown"], vec!["--console", "--service"]] {
        assert_eq!(
            select_entry_mode(arguments).unwrap_err().code(),
            "service_mode_invalid"
        );
    }
}

#[test]
fn rotation_retains_only_the_configured_completed_files() {
    assert_eq!(RotationPolicy::default().max_bytes, 20 * 1024 * 1024);
    assert_eq!(RotationPolicy::default().retained_files, 10);
    let directory = tempdir().unwrap();
    let mut logger = JsonLogger::new(
        directory.path(),
        RotationPolicy {
            max_bytes: 180,
            retained_files: 2,
        },
    )
    .unwrap();
    for _ in 0..12 {
        logger.write(&event()).unwrap();
    }

    let names: Vec<_> = std::fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(names.iter().any(|name| name == "cellar.log"));
    assert!(names.len() <= 3, "current plus two completed logs");
    assert!(!directory.path().join("cellar.log.3").exists());
}

struct FailingWriter;

impl Write for FailingWriter {
    fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("contains C:\\secret and bearer token"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn writer_failures_are_stable_and_redacted() {
    let error = write_json(&mut FailingWriter, &event()).unwrap_err();
    assert_eq!(error.code(), "log_write_failed");
    assert_eq!(format!("{error}"), "log_write_failed");
    assert_eq!(format!("{error:?}"), "log_write_failed");
}

#[cfg(windows)]
#[test]
#[ignore = "acceptance: requires an installed Event Log source and service identity privileges"]
fn fatal_event_log_acceptance() {
    use cellar_service::logging::{FatalEvent, WindowsEventLog};

    WindowsEventLog::new("Cellar")
        .record(&FatalEvent::new("startup_failed", SanitizedContext::new()))
        .unwrap();
}
