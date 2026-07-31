# Cellar Phase 1 Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Produce a secure, installable Cellar service foundation with configuration, origin TLS, SQLite migrations, Cloudflare authentication, owner enrollment, health endpoints, and NTFS storage preflight.

**Architecture:** Create the modular Rust workspace and composition root first. Keep the authenticated HTTPS origin and unproxied health listener separate, put Windows behavior behind adapters, and make configuration/readiness fail closed until enrollment and storage checks succeed.

**Tech Stack:** Rust 1.93+, Axum 0.8, Tokio 1.53, SQLx 0.9 SQLite, rustls 0.23, rcgen 0.14, jsonwebtoken 11, reqwest 0.13, windows 0.62, windows-service 0.8, tracing.

---

## Parallel Execution

| Wave | Task | `depends_on` | Exclusive ownership domain |
|---|---|---|---|
| 0 | P1-T1 | none | workspace manifests, crate entry points, `.gitignore` |
| 1 | P1-T2 | P1-T1 | `cellar-core` identifiers, errors, ports |
| 1 | P1-T3 | P1-T1 | `cellar-config` |
| 2 | P1-T4 | P1-T2 | migrations and `cellar-db` pool/migration modules |
| 2 | P1-T5 | P1-T3 | TLS module and Windows key ACL |
| 2 | P1-T6 | P1-T3 | `cellar-auth` JWT/JWKS middleware |
| 3 | P1-T7 | P1-T3, P1-T4, P1-T6 | enrollment, CSRF, session route |
| 4 | P1-T8 | P1-T2 through P1-T7 | Windows preflight/service adapter and service composition |

Tasks in the same wave may run concurrently. The coordinator integrates them in task-ID order and runs `cargo test --workspace` after each wave; Wave 4 then runs the full Gate 1 commands from the roadmap. Task file lists below are exclusive: any required Cargo manifest or shared `lib.rs` edit not already listed must be reserved by the coordinator before editing.

### Task 1: Scaffold the Rust workspace and quality gates

**Files:**
- Create: `Cargo.toml`
- Create: `rust-toolchain.toml`
- Create: `crates/cellar-{core,config,db,storage,windows,auth,api,service,test-support,e2e}/Cargo.toml`
- Create: `crates/cellar-{core,config,db,storage,windows,auth,api}/src/lib.rs`
- Create: `crates/cellar-service/src/main.rs`
- Modify: `.gitignore`

- [ ] **Step 1: Create empty library and binary crates**

```powershell
$libs = 'core','config','db','storage','windows','auth','api','test-support','e2e'
foreach ($name in $libs) {
  cargo new "crates/cellar-$name" --lib
}
cargo new crates/cellar-service --bin
```

- [ ] **Step 2: Lock the workspace manifest**

```toml
[workspace]
resolver = "2"
members = ["crates/*"]

[workspace.package]
edition = "2024"
rust-version = "1.93"
license = "MIT"

[workspace.dependencies]
axum = "0.8.9"
tokio = { version = "1.53.1", features = ["full"] }
serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1"
thiserror = "2"
tracing = "0.1"
tracing-subscriber = { version = "0.3.23", features = ["env-filter", "json"] }
uuid = { version = "1.24.0", features = ["v7", "serde"] }
time = { version = "0.3.54", features = ["serde", "formatting", "parsing"] }
```

- [ ] **Step 2a: Ignore generated and secret material**

```gitignore
.superpowers/
/target/
/artifacts/
/web/node_modules/
/web/dist/
*.db
*.db-shm
*.db-wal
*.key
*.pfx
.env
```

- [ ] **Step 3: Pin the toolchain**

```toml
[toolchain]
channel = "1.93.0"
components = ["clippy", "rustfmt"]
profile = "minimal"
```

- [ ] **Step 4: Verify the empty workspace**

Run:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: all commands exit `0`.

- [ ] **Step 5: Commit**

```powershell
git add Cargo.toml rust-toolchain.toml crates .gitignore
git commit -m "build: scaffold Rust workspace"
```

### Task 2: Define core identifiers, readiness, and errors

**Files:**
- Create: `crates/cellar-core/src/ids.rs`
- Create: `crates/cellar-core/src/error.rs`
- Create: `crates/cellar-core/src/ports.rs`
- Modify: `crates/cellar-core/src/lib.rs`
- Test: `crates/cellar-core/tests/core_types.rs`

- [ ] **Step 1: Write failing identifier and error tests**

```rust
use cellar_core::{ProjectId, ReadinessBlocker};

#[test]
fn project_ids_round_trip() {
    let id = ProjectId::new();
    assert_eq!(id, id.to_string().parse().unwrap());
}

#[test]
fn readiness_codes_are_stable() {
    assert_eq!(
        ReadinessBlocker::OwnerEnrollmentRequired.code(),
        "owner_enrollment_required"
    );
}
```

- [ ] **Step 2: Run the test and verify failure**

Run: `cargo test -p cellar-core --test core_types`

Expected: FAIL because the public types do not exist.

- [ ] **Step 3: Add typed IDs and stable readiness codes**

```rust
macro_rules! define_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Serialize, serde::Deserialize)]
        pub struct $name(uuid::Uuid);

        impl $name {
            pub fn new() -> Self { Self(uuid::Uuid::now_v7()) }
        }
    };
}

define_id!(ProjectId);
define_id!(FileEntryId);
define_id!(UploadId);
define_id!(OperationId);
define_id!(TrashId);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadinessBlocker {
    ConfigurationRequired,
    OwnerEnrollmentRequired,
    MigrationRequired,
    RecoveryRequired,
    StorageUnavailable,
    ReconciliationRequired,
}

impl ReadinessBlocker {
    pub const fn code(self) -> &'static str {
        match self {
            Self::ConfigurationRequired => "configuration_required",
            Self::OwnerEnrollmentRequired => "owner_enrollment_required",
            Self::MigrationRequired => "migration_required",
            Self::RecoveryRequired => "recovery_required",
            Self::StorageUnavailable => "storage_unavailable",
            Self::ReconciliationRequired => "reconciliation_required",
        }
    }
}
```

Implement `Display` and `FromStr` for each ID type in `ids.rs`. Define `CellarError` variants for invalid input, unauthenticated, forbidden, conflict, range, storage full, unavailable, and internal errors.

- [ ] **Step 4: Run core checks**

Run: `cargo test -p cellar-core`

Expected: PASS.

- [ ] **Step 5: Commit**

```powershell
git add crates/cellar-core
git commit -m "feat: define core identifiers and readiness"
```

### Task 3: Implement fail-closed configuration and owner bootstrap

**Files:**
- Create: `crates/cellar-config/src/model.rs`
- Create: `crates/cellar-config/src/validate.rs`
- Create: `crates/cellar-config/src/store.rs`
- Modify: `crates/cellar-config/src/lib.rs`
- Test: `crates/cellar-config/tests/config_validation.rs`

- [ ] **Step 1: Write failing validation tests**

```rust
#[test]
fn requires_https_canonical_origin() {
    let cfg = fixture().with_external_origin("http://cellar.example.com");
    assert_eq!(cfg.validate().unwrap_err().code(), "external_origin_must_be_https");
}

#[test]
fn origin_and_health_ports_must_differ() {
    let cfg = fixture().with_ports(9443, 9443);
    assert_eq!(cfg.validate().unwrap_err().code(), "listener_ports_conflict");
}
```

- [ ] **Step 2: Verify the tests fail**

Run: `cargo test -p cellar-config --test config_validation`

Expected: FAIL because configuration types are absent.

- [ ] **Step 3: Implement the configuration model**

```rust
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct CellarConfig {
    pub external_origin: url::Url,
    pub team_domain: url::Url,
    pub aud_tags: Vec<String>,
    pub bootstrap_owner_email: Option<String>,
    pub owner_subject: Option<String>,
    pub storage_root: std::path::PathBuf,
    pub origin_port: u16,
    pub health_port: u16,
}

impl CellarConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.external_origin.scheme() != "https" {
            return Err(ConfigError::ExternalOriginMustBeHttps);
        }
        if self.origin_port == self.health_port {
            return Err(ConfigError::ListenerPortsConflict);
        }
        if self.aud_tags.is_empty() {
            return Err(ConfigError::AudienceRequired);
        }
        Ok(())
    }
}
```

Use atomic temp-file write, flush, and rename for `config.toml`. Store only a hash of the 256-bit claim code and its expiry.

- [ ] **Step 4: Test validation and atomic persistence**

Run: `cargo test -p cellar-config`

Expected: PASS, including a test that interrupted temp files never replace the last valid config.

- [ ] **Step 5: Commit**

```powershell
git add crates/cellar-config
git commit -m "feat: add fail-closed configuration"
```

### Task 4: Create the SQLite pool, migrations, and constraints

**Files:**
- Create: `migrations/0001_initial.sql`
- Create: `migrations/0002_indexes.sql`
- Create: `crates/cellar-db/src/pool.rs`
- Create: `crates/cellar-db/src/migrate.rs`
- Modify: `crates/cellar-db/src/lib.rs`
- Test: `crates/cellar-db/tests/migrations.rs`

- [ ] **Step 1: Write failing migration tests**

```rust
#[tokio::test]
async fn enables_required_pragmas_and_rejects_duplicate_live_names() {
    let db = TestDb::new().await;
    assert_eq!(db.pragma("journal_mode").await, "wal");
    assert_eq!(db.pragma("synchronous").await, "2");
    assert_eq!(db.pragma("foreign_keys").await, "1");
    assert!(db.insert_duplicate_windows_name().await.is_err());
}
```

- [ ] **Step 2: Run and verify failure**

Run: `cargo test -p cellar-db --test migrations`

Expected: FAIL because the pool and migrations are absent.

- [ ] **Step 3: Implement the connection contract**

```rust
pub async fn connect(path: &Path) -> Result<SqlitePool, DbError> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Full)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));

    SqlitePoolOptions::new()
        .max_connections(4)
        .after_connect(|conn, _| Box::pin(async move {
            register_windows_ordinal_collation(conn)?;
            Ok(())
        }))
        .connect_with(options)
        .await
        .map_err(DbError::from)
}
```

Create the exact tables, checks, composite foreign keys, and partial unique indexes from design sections 6.1–6.8.

- [ ] **Step 4: Verify schema behavior**

Run: `cargo test -p cellar-db`

Expected: PASS for pragma, FK, state CHECK, root-name, child-name, active-upload reservation, cover-project, and pending-chunk constraints.

- [ ] **Step 5: Commit**

```powershell
git add migrations crates/cellar-db
git commit -m "feat: add SQLite schema and migrations"
```

### Task 5: Generate and serve the authenticated origin TLS certificate

**Files:**
- Create: `crates/cellar-service/src/tls.rs`
- Create: `crates/cellar-windows/src/acl.rs`
- Test: `crates/cellar-service/tests/origin_tls.rs`
- Test: `crates/cellar-windows/tests/key_acl.rs`

- [ ] **Step 1: Write failing certificate lifecycle tests**

```rust
#[test]
fn leaf_is_signed_by_local_ca_and_names_cellar_local() {
    let bundle = TestCertificates::generate();
    bundle.verify_chain("cellar.local").unwrap();
    assert!(bundle.leaf_days_remaining() >= 330);
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p cellar-service --test origin_tls`

Expected: FAIL because certificate generation is absent.

- [ ] **Step 3: Implement CA and leaf lifecycle**

```rust
pub struct OriginTlsPaths {
    pub ca_cert: PathBuf,
    pub ca_key: PathBuf,
    pub leaf_cert: PathBuf,
    pub leaf_key: PathBuf,
}

pub fn ensure_origin_tls(paths: &OriginTlsPaths, now: OffsetDateTime)
    -> Result<TlsMaterial, TlsError>
{
    // Create five-year CA when absent.
    // Create or renew one-year cellar.local leaf within 30 days of expiry.
    // Atomically persist, then apply restricted ACLs before returning.
}
```

Use `rcgen` for generation and `rustls` for serving. Apply key ACLs to `SYSTEM`, `Administrators`, and `NT SERVICE\Cellar` only.

- [ ] **Step 4: Test chain, renewal, rollback, and ACL**

Run:

```powershell
cargo test -p cellar-service --test origin_tls
cargo test -p cellar-windows --test key_acl
```

Expected: PASS.

- [ ] **Step 5: Commit**

```powershell
git add crates/cellar-service/src/tls.rs crates/cellar-service/tests crates/cellar-windows
git commit -m "feat: add authenticated origin TLS"
```

### Task 6: Implement Access JWT validation and bounded JWKS refresh

**Files:**
- Create: `crates/cellar-auth/src/claims.rs`
- Create: `crates/cellar-auth/src/jwks.rs`
- Create: `crates/cellar-auth/src/middleware.rs`
- Modify: `crates/cellar-auth/src/lib.rs`
- Test: `crates/cellar-auth/tests/access_jwt.rs`

- [ ] **Step 1: Write failing JWT matrix tests**

```rust
#[tokio::test]
async fn accepts_only_expected_app_owner() {
    assert!(validator().validate(valid_owner_token()).await.is_ok());
    assert_code(wrong_issuer(), "invalid_issuer").await;
    assert_code(wrong_audience(), "invalid_audience").await;
    assert_code(service_token(), "service_token_forbidden").await;
    assert_code(duplicate_header(), "duplicate_access_token").await;
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p cellar-auth --test access_jwt`

Expected: FAIL because the validator does not exist.

- [ ] **Step 3: Implement exact validation**

```rust
#[derive(Debug, serde::Deserialize)]
pub struct AccessClaims {
    pub iss: String,
    pub aud: Vec<String>,
    pub sub: String,
    pub email: Option<String>,
    pub exp: i64,
    pub nbf: i64,
    pub iat: i64,
    #[serde(rename = "type")]
    pub token_type: String,
}

pub enum OwnerMode<'a> {
    Unenrolled { bootstrap_email: &'a str },
    Enrolled { owner_subject: &'a str },
}
```

Use a bounded token length, exact `RS256`, exact issuer, configured audience membership, app token type, clock skew, non-empty subject, and owner-mode checks. Implement one in-flight JWKS refresh with timeout, response-size limit, and rate limit.

- [ ] **Step 4: Run auth tests**

Run: `cargo test -p cellar-auth`

Expected: PASS for valid, expired, nbf, iat, issuer, audience, type, subject, owner, unknown-kid, oversize, duplicate, and refresh-storm cases.

- [ ] **Step 5: Commit**

```powershell
git add crates/cellar-auth
git commit -m "feat: validate Cloudflare Access identity"
```

### Task 7: Add enrollment and session-bound CSRF

**Files:**
- Create: `crates/cellar-auth/src/enrollment.rs`
- Create: `crates/cellar-auth/src/csrf.rs`
- Create: `crates/cellar-api/src/routes/session.rs`
- Create: `crates/cellar-api/tests/enrollment.rs`
- Create: `crates/cellar-api/tests/csrf.rs`

- [ ] **Step 1: Write failing mode-transition tests**

```rust
#[tokio::test]
async fn claim_requires_email_origin_and_one_time_code() {
    let app = unenrolled_app().await;
    assert_eq!(claim(&app, wrong_email()).await.status(), 403);
    assert_eq!(claim(&app, wrong_origin()).await.status(), 403);
    assert_eq!(claim(&app, wrong_code()).await.status(), 403);
    assert_eq!(claim(&app, valid_claim()).await.status(), 204);
    assert_eq!(claim(&app, valid_claim()).await.status(), 404);
}
```

- [ ] **Step 2: Verify failure**

Run: `cargo test -p cellar-api --test enrollment --test csrf`

Expected: FAIL because routes and CSRF storage are absent.

- [ ] **Step 3: Implement explicit middleware modes**

```rust
pub fn route_access(mode: EnrollmentMode, path: &str) -> RouteAccess {
    match (mode, path) {
        (EnrollmentMode::Unenrolled, "/owner/claim") => RouteAccess::ClaimOnly,
        (EnrollmentMode::Unenrolled, _) => RouteAccess::Deny,
        (EnrollmentMode::Enrolled, "/owner/claim") => RouteAccess::NotFound,
        (EnrollmentMode::Enrolled, _) => RouteAccess::OwnerOnly,
    }
}
```

Generate 256-bit random CSRF values, store only hashes, bind them to subject and Access iat/exp, rotate after eight hours or restart, and require exact canonical Origin. The claim route uses Access JWT, email, Origin, and one-time claim code instead of session CSRF.

- [ ] **Step 4: Run enrollment and CSRF tests**

Run: `cargo test -p cellar-api --test enrollment --test csrf`

Expected: PASS.

- [ ] **Step 5: Commit**

```powershell
git add crates/cellar-auth crates/cellar-api
git commit -m "feat: add owner enrollment and CSRF"
```

### Task 8: Add Windows preflight, service host, health listener, and logging

**Files:**
- Create: `crates/cellar-windows/src/preflight.rs`
- Create: `crates/cellar-windows/src/service.rs`
- Create: `crates/cellar-service/src/app.rs`
- Create: `crates/cellar-service/src/logging.rs`
- Create: `crates/cellar-api/src/health.rs`
- Modify: `crates/cellar-service/src/main.rs`
- Test: `crates/cellar-windows/tests/storage_preflight.rs`
- Test: `crates/cellar-service/tests/listeners.rs`

- [ ] **Step 1: Write failing preflight and listener tests**

```rust
#[test]
fn rejects_non_ntfs_and_reparse_roots() {
    assert_eq!(preflight(fake_exfat()).unwrap_err().code(), "ntfs_required");
    assert_eq!(preflight(fake_reparse()).unwrap_err().code(), "reparse_root_forbidden");
}

#[tokio::test]
async fn health_listener_is_separate_from_authenticated_origin() {
    let app = TestService::start().await;
    assert_eq!(app.health_get("/health/live").await.status(), 200);
    assert_eq!(app.origin_get("/health/live").await.status(), 401);
}
```

- [ ] **Step 2: Verify failure**

Run:

```powershell
cargo test -p cellar-windows --test storage_preflight
cargo test -p cellar-service --test listeners
```

Expected: FAIL.

- [ ] **Step 3: Implement preflight and composition root**

```rust
pub async fn run(config: CellarConfig) -> anyhow::Result<()> {
    let preflight = cellar_windows::preflight::run_as_service(&config.storage_root)?;
    let db = cellar_db::connect(program_data_db()).await?;
    let readiness = recover_and_reconcile(&db, &preflight).await?;

    tokio::try_join!(
        serve_health(config.health_port, readiness.clone()),
        serve_authenticated_tls(config.origin_port, build_router(db, readiness))
    )?;
    Ok(())
}
```

Preflight performs create, write, `FlushFileBuffers`, no-replace rename, and delete on the real volume. Bind both listeners exclusively. Emit JSON logs with rotation and redaction; write fatal startup failures to Windows Event Log.

- [ ] **Step 4: Run Phase 1 verification**

Run:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: PASS.

- [ ] **Step 5: Commit**

```powershell
git add crates/cellar-windows crates/cellar-service crates/cellar-api
git commit -m "feat: add Windows service foundation"
```
