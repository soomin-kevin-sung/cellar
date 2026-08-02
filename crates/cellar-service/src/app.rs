use std::cmp::Ordering;
use std::fmt;
use std::future::IntoFuture;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener as StdTcpListener};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderName, Method, StatusCode};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum_server::Handle;
use axum_server::tls_rustls::RustlsConfig;
use cellar_api::health::{Readiness, health_router};
use cellar_api::routes::files::{
    DownloadSource, FileMutationSource, files_router_with_downloads, files_router_with_services,
};
use cellar_api::routes::session::{
    RequestId, SessionState, prepare_request_id, session_router_with_routes, shared_error_response,
};
use cellar_api::uploads_router;
use cellar_auth::{
    AccessClaims, AccessValidator, AccessValidatorConfig, AuthError, CsrfManager,
    EnrollmentService, FileEnrollmentStore, JwksFetchError, JwksFetcher, JwksResponse, OwnerMode,
    select_access_jwt_header,
};
use cellar_config::CellarConfig;
use cellar_core::{FileService, ReadinessBlocker, UploadLimits, UploadService, UploadStagingStore};
use cellar_db::{FilenameCollation, SqliteUploadRepository};
use cellar_windows::service::{NORMAL_STOP_TARGET, PRESHUTDOWN_BUDGET, ServiceControl};
use cellar_windows::{WindowsStorage, WindowsUploadStaging};
use futures_util::TryStreamExt;
use rustls::ServerConfig;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::sync::watch;
use tokio_util::io::StreamReader;

use crate::downloads::{
    BoundedReconciliationScheduler, DEFAULT_RECONCILIATION_QUEUE_CAPACITY,
    ProductionDownloadSource, SqliteDownloadCatalog, WindowsDownloadPlatform,
};
use crate::tls::{OriginTlsPaths, TlsMaterial, ensure_origin_tls};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ListenerConfig {
    pub origin_port: u16,
    pub health_port: u16,
}

pub struct BoundListeners {
    origin: StdTcpListener,
    health: StdTcpListener,
}

impl fmt::Debug for BoundListeners {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BoundListeners")
            .field("origin", &self.origin.local_addr().ok())
            .field("health", &self.health.local_addr().ok())
            .finish()
    }
}

impl BoundListeners {
    #[must_use]
    pub fn origin_addr(&self) -> std::net::SocketAddr {
        self.origin
            .local_addr()
            .expect("a bound listener always has a local address")
    }

    #[must_use]
    pub fn health_addr(&self) -> std::net::SocketAddr {
        self.health
            .local_addr()
            .expect("a bound listener always has a local address")
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum ListenerError {
    InvalidConfiguration,
    BindFailed,
    ServeFailed,
}

impl ListenerError {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidConfiguration => "listener_config_invalid",
            Self::BindFailed => "listener_bind_failed",
            Self::ServeFailed => "listener_serve_failed",
        }
    }
}

impl fmt::Debug for ListenerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl fmt::Display for ListenerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for ListenerError {}

pub async fn bind_listeners(config: ListenerConfig) -> Result<BoundListeners, ListenerError> {
    if config.origin_port == 0
        || config.health_port == 0
        || config.origin_port == config.health_port
    {
        return Err(ListenerError::InvalidConfiguration);
    }
    let origin = bind_one(config.origin_port)?;
    let health = match bind_one(config.health_port) {
        Ok(listener) => listener,
        Err(error) => {
            drop(origin);
            return Err(error);
        }
    };
    Ok(BoundListeners { origin, health })
}

fn bind_one(port: u16) -> Result<StdTcpListener, ListenerError> {
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))
        .map_err(|_| ListenerError::BindFailed)?;
    set_exclusive(&socket)?;
    socket
        .set_nonblocking(true)
        .map_err(|_| ListenerError::BindFailed)?;
    socket
        .bind(&SocketAddrV4::new(Ipv4Addr::LOCALHOST, port).into())
        .map_err(|_| ListenerError::BindFailed)?;
    socket.listen(128).map_err(|_| ListenerError::BindFailed)?;
    Ok(socket.into())
}

#[cfg(windows)]
fn set_exclusive(socket: &Socket) -> Result<(), ListenerError> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        SO_EXCLUSIVEADDRUSE, SOCKET_ERROR, SOL_SOCKET, setsockopt,
    };

    let exclusive = 1_i32;
    // SAFETY: `socket` owns a live Winsock socket and `exclusive` is readable
    // for the exact option length during this synchronous call.
    let result = unsafe {
        setsockopt(
            socket.as_raw_socket() as usize,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            (&exclusive as *const i32).cast(),
            size_of::<i32>() as i32,
        )
    };
    if result == SOCKET_ERROR {
        Err(ListenerError::BindFailed)
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn set_exclusive(socket: &Socket) -> Result<(), ListenerError> {
    socket
        .set_reuse_address(false)
        .map_err(|_| ListenerError::BindFailed)
}

pub fn origin_router() -> Router {
    Router::new().fallback(|| async { StatusCode::UNAUTHORIZED })
}

const ACCESS_JWT_HEADER: HeaderName = HeaderName::from_static("cf-access-jwt-assertion");

#[async_trait]
pub trait OriginAuthenticator: Send + Sync + 'static {
    async fn validate(
        &self,
        token: &str,
        owner_mode: OwnerMode<'_>,
    ) -> Result<AccessClaims, AuthError>;
    fn max_token_len(&self) -> usize;
}

struct CloudflareAuthenticator {
    validator: AccessValidator,
    max_token_len: usize,
}

#[async_trait]
impl OriginAuthenticator for CloudflareAuthenticator {
    async fn validate(
        &self,
        token: &str,
        owner_mode: OwnerMode<'_>,
    ) -> Result<AccessClaims, AuthError> {
        self.validator.validate(token, owner_mode).await
    }

    fn max_token_len(&self) -> usize {
        self.max_token_len
    }
}

struct ReqwestJwksFetcher {
    client: reqwest::Client,
}

#[async_trait]
impl JwksFetcher for ReqwestJwksFetcher {
    async fn fetch(&self, url: &Url) -> Result<JwksResponse, JwksFetchError> {
        let response = self
            .client
            .get(url.clone())
            .send()
            .await
            .map_err(|_| JwksFetchError::unavailable())?;
        let status = response.status().as_u16();
        let stream = response
            .bytes_stream()
            .map_err(|_| std::io::Error::other("jwks stream unavailable"));
        Ok(JwksResponse::new(status, StreamReader::new(stream), None))
    }
}

use url::Url;

struct OriginAuthState {
    authenticator: Arc<dyn OriginAuthenticator>,
    config_path: PathBuf,
    readiness: Readiness,
}

pub fn origin_router_with_authenticator(
    config_path: &std::path::Path,
    authenticator: Arc<dyn OriginAuthenticator>,
    readiness: Readiness,
    shutdown: Shutdown,
) -> Router {
    origin_router_with_authenticator_and_routes(
        config_path,
        authenticator,
        readiness,
        shutdown,
        Router::new(),
    )
}

fn origin_router_with_authenticator_and_routes(
    config_path: &std::path::Path,
    authenticator: Arc<dyn OriginAuthenticator>,
    readiness: Readiness,
    shutdown: Shutdown,
    protected_routes: Router<SessionState<FileEnrollmentStore>>,
) -> Router {
    let store = Arc::new(FileEnrollmentStore::new(config_path));
    let enrollment = EnrollmentService::new(Arc::clone(&store));
    let auth_state = Arc::new(OriginAuthState {
        authenticator,
        config_path: config_path.to_path_buf(),
        readiness,
    });
    session_router_with_routes(
        enrollment,
        Arc::new(CsrfManager::new()),
        unix_now,
        protected_routes,
    )
    .layer(from_fn_with_state(shutdown, mutation_shutdown_boundary))
    .layer(from_fn_with_state(auth_state, access_boundary))
}

pub fn origin_router_with_authenticator_and_uploads(
    config_path: &std::path::Path,
    authenticator: Arc<dyn OriginAuthenticator>,
    readiness: Readiness,
    shutdown: Shutdown,
    upload_service: UploadService,
) -> Router {
    origin_router_with_authenticator_and_routes(
        config_path,
        authenticator,
        readiness,
        shutdown,
        uploads_router::<FileEnrollmentStore>(upload_service),
    )
}

pub fn origin_router_with_authenticator_and_services(
    config_path: &std::path::Path,
    authenticator: Arc<dyn OriginAuthenticator>,
    readiness: Readiness,
    shutdown: Shutdown,
    upload_service: UploadService,
    file_service: FileService,
    download_source: Arc<dyn DownloadSource>,
) -> Router {
    let protected =
        uploads_router::<FileEnrollmentStore>(upload_service).merge(files_router_with_downloads::<
            FileEnrollmentStore,
        >(
            file_service,
            download_source,
        ));
    origin_router_with_authenticator_and_routes(
        config_path,
        authenticator,
        readiness,
        shutdown,
        protected,
    )
}

pub struct OriginFileMutationServices {
    pub uploads: UploadService,
    pub files: FileService,
    pub downloads: Arc<dyn DownloadSource>,
    pub mutations: Arc<dyn FileMutationSource>,
}

pub fn origin_router_with_authenticator_and_file_mutations(
    config_path: &std::path::Path,
    authenticator: Arc<dyn OriginAuthenticator>,
    readiness: Readiness,
    shutdown: Shutdown,
    services: OriginFileMutationServices,
) -> Router {
    let protected = uploads_router::<FileEnrollmentStore>(services.uploads).merge(
        files_router_with_services::<FileEnrollmentStore>(
            services.files,
            services.downloads,
            services.mutations,
        ),
    );
    origin_router_with_authenticator_and_routes(
        config_path,
        authenticator,
        readiness,
        shutdown,
        protected,
    )
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|value| i64::try_from(value.as_secs()).ok())
        .unwrap_or(0)
}

async fn access_boundary(
    State(state): State<Arc<OriginAuthState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let (request_id, _) = match prepare_request_id(&mut request) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let token = match select_access_jwt_header(
        request
            .headers()
            .get_all(&ACCESS_JWT_HEADER)
            .iter()
            .map(|value| value.as_bytes()),
        state.authenticator.max_token_len(),
    ) {
        Ok(token) => token,
        Err(error) => return origin_auth_error(error.code(), request_id),
    };
    let persisted = match cellar_config::load_config(&state.config_path) {
        Ok(persisted) => persisted,
        Err(_) => return origin_auth_error("authentication_unavailable", request_id),
    };
    let owner_mode = match (
        persisted.config.bootstrap_owner_email.as_deref(),
        persisted.config.owner_subject.as_deref(),
    ) {
        (Some(email), None) => OwnerMode::Unenrolled {
            bootstrap_email: email,
        },
        (None, Some(subject)) => OwnerMode::Enrolled {
            owner_subject: subject,
        },
        _ => return origin_auth_error("authentication_unavailable", request_id),
    };
    let claims = match state.authenticator.validate(token, owner_mode).await {
        Ok(claims) => claims,
        Err(error) => return origin_auth_error(error.code(), request_id),
    };
    request.extensions_mut().insert(claims);
    let response = next.run(request).await;
    if cellar_config::load_config(&state.config_path)
        .ok()
        .is_some_and(|persisted| persisted.config.owner_subject.is_some())
    {
        state
            .readiness
            .clear(ReadinessBlocker::OwnerEnrollmentRequired);
    }
    response
}

fn origin_auth_error(code: &'static str, request_id: RequestId) -> Response {
    shared_error_response(
        StatusCode::UNAUTHORIZED,
        code,
        "Authentication could not be completed.",
        request_id,
    )
}

async fn mutation_shutdown_boundary(
    State(shutdown): State<Shutdown>,
    mut request: Request,
    next: Next,
) -> Response {
    let (request_id, _) = match prepare_request_id(&mut request) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    if is_unsafe_method(request.method()) && !shutdown.accepting_mutations() {
        return shared_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "service_stopping",
            "The service is stopping and cannot accept mutations.",
            request_id,
        );
    }
    next.run(request).await
}

fn is_unsafe_method(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

fn production_authenticator(
    config: &CellarConfig,
) -> Result<Arc<dyn OriginAuthenticator>, AppError> {
    let client = reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| AppError::Authentication)?;
    let fetcher: Arc<dyn JwksFetcher> = Arc::new(ReqwestJwksFetcher { client });
    build_origin_authenticator(config, fetcher)
}

fn build_origin_authenticator(
    config: &CellarConfig,
    fetcher: Arc<dyn JwksFetcher>,
) -> Result<Arc<dyn OriginAuthenticator>, AppError> {
    let issuer = config.team_domain.origin().ascii_serialization();
    let jwks_url = config
        .team_domain
        .join("/cdn-cgi/access/certs")
        .map_err(|_| AppError::Authentication)?;
    let validator_config = AccessValidatorConfig::new_with_audiences(
        &issuer,
        config.aud_tags.iter().cloned(),
        jwks_url.as_str(),
    )
    .map_err(|_| AppError::Authentication)?;
    let max_token_len = validator_config.max_token_len();
    Ok(Arc::new(CloudflareAuthenticator {
        validator: AccessValidator::new(validator_config, fetcher),
        max_token_len,
    }))
}

#[derive(Clone)]
pub struct Shutdown {
    inner: Arc<ShutdownInner>,
}

struct ShutdownInner {
    accepting_mutations: AtomicBool,
    sender: watch::Sender<Option<ServiceControl>>,
}

impl Shutdown {
    #[must_use]
    pub fn new() -> Self {
        let (sender, _) = watch::channel(None);
        Self {
            inner: Arc::new(ShutdownInner {
                accepting_mutations: AtomicBool::new(true),
                sender,
            }),
        }
    }

    pub fn signal(&self, control: ServiceControl) {
        self.inner
            .accepting_mutations
            .store(false, AtomicOrdering::Release);
        self.inner.sender.send_replace(Some(control));
    }

    pub async fn wait(&self) -> ServiceControl {
        let mut receiver = self.inner.sender.subscribe();
        loop {
            if let Some(control) = *receiver.borrow_and_update() {
                return control;
            }
            if receiver.changed().await.is_err() {
                return ServiceControl::Stop;
            }
        }
    }

    #[must_use]
    pub fn accepting_mutations(&self) -> bool {
        self.inner.accepting_mutations.load(AtomicOrdering::Acquire)
    }

    #[must_use]
    pub const fn normal_stop_target(&self) -> Duration {
        NORMAL_STOP_TARGET
    }

    #[must_use]
    pub const fn preshutdown_budget(&self) -> Duration {
        PRESHUTDOWN_BUDGET
    }
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StartupGates {
    pub recovery_complete: bool,
    pub reconciliation_complete: bool,
}

pub struct RunOptions {
    pub config: CellarConfig,
    pub config_path: PathBuf,
    pub database_path: PathBuf,
    pub tls_paths: OriginTlsPaths,
    pub startup_gates: Option<StartupGates>,
    pub on_started: Option<StartedCallback>,
    pub shutdown: Shutdown,
}

pub type StartedCallback = Box<dyn FnOnce() -> Result<(), ListenerError> + Send>;

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum AppError {
    Configuration,
    StoragePreflight,
    Database,
    Tls,
    Listener,
    Authentication,
    Recovery,
    Reconciliation,
}

impl AppError {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Configuration => "startup_configuration_failed",
            Self::StoragePreflight => "startup_storage_preflight_failed",
            Self::Database => "startup_database_failed",
            Self::Tls => "startup_tls_failed",
            Self::Listener => "startup_listener_failed",
            Self::Authentication => "startup_authentication_failed",
            Self::Recovery => "startup_recovery_failed",
            Self::Reconciliation => "startup_reconciliation_failed",
        }
    }
}

impl fmt::Debug for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for AppError {}

pub async fn run(mut options: RunOptions) -> Result<(), AppError> {
    options
        .config
        .validate()
        .map_err(|_| AppError::Configuration)?;
    let readiness = Readiness::all_blocked();
    readiness.clear(ReadinessBlocker::ConfigurationRequired);
    if options.config.owner_subject.is_some() {
        readiness.clear(ReadinessBlocker::OwnerEnrollmentRequired);
    }

    let identity = cellar_windows::preflight::open_as_service(&options.config.storage_root)
        .map_err(|_| AppError::StoragePreflight)?;
    let storage = WindowsStorage::adopt(identity).map_err(|_| AppError::StoragePreflight)?;
    let staging = Arc::new(
        WindowsUploadStaging::open(storage.clone()).map_err(|_| AppError::StoragePreflight)?,
    );
    readiness.clear(ReadinessBlocker::StorageUnavailable);

    let database_directory = options.database_path.parent().ok_or(AppError::Database)?;
    std::fs::create_dir_all(database_directory).map_err(|_| AppError::Database)?;
    let pool = cellar_db::open_pool(
        &options.database_path,
        FilenameCollation::windows_ordinal_ci_v1(compare_filename_ordinal),
    )
    .await
    .map_err(|_| AppError::Database)?;
    cellar_db::migrate(&pool)
        .await
        .map_err(|_| AppError::Database)?;
    readiness.clear(ReadinessBlocker::MigrationRequired);
    let (reconciliation_scheduler, mut reconciliation_requests) =
        BoundedReconciliationScheduler::channel(DEFAULT_RECONCILIATION_QUEUE_CAPACITY);
    let project_mutations = Arc::new(cellar_core::InMemoryProjectMutationCoordinator::default());
    let mutation_source = Arc::new(
        crate::file_mutations::ProductionFileMutationSource::open_with_reconciliation(
            pool.clone(),
            storage.clone(),
            reconciliation_scheduler.clone(),
            project_mutations.clone(),
        )
        .await
        .map_err(|_| AppError::Recovery)?,
    );
    crate::recovery::initialize_file_mutation_recovery(&mutation_source).await?;
    let upload_service = crate::recovery::initialize_upload_finalization_recovery_with_coordinator(
        &pool,
        staging.clone(),
        staging,
        &readiness,
        time::OffsetDateTime::now_utc(),
        project_mutations,
    )
    .await?;
    let startup_gates = match options.startup_gates {
        Some(gates) => gates,
        None => check_startup_gates(&pool).await?,
    };
    if !startup_gates.recovery_complete {
        readiness.block(ReadinessBlocker::RecoveryRequired);
    }
    apply_reconciliation_startup_gate(
        &readiness,
        &startup_gates,
        !reconciliation_requests.is_empty(),
    );
    let reconciliation_readiness = readiness.clone();
    let reconciliation_task = tokio::spawn(async move {
        while reconciliation_requests.recv().await.is_some() {
            reconciliation_readiness.block(ReadinessBlocker::ReconciliationRequired);
        }
    });

    let tls = ensure_origin_tls(&options.tls_paths, time::OffsetDateTime::now_utc())
        .map_err(|_| AppError::Tls)?;
    sync_origin_trust_readiness(&readiness, &tls);
    let listeners = bind_listeners(ListenerConfig {
        origin_port: options.config.origin_port,
        health_port: options.config.health_port,
    })
    .await
    .map_err(|_| AppError::Listener)?;

    let server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            tls.certificate_chain().to_vec(),
            tls.private_key().clone_key(),
        )
        .map_err(|_| AppError::Tls)?;
    let rustls = RustlsConfig::from_config(Arc::new(server_config));
    let file_service =
        FileService::new(Arc::new(cellar_db::SqliteFileRepository::new(pool.clone())));
    let download_source = Arc::new(ProductionDownloadSource::new(
        Arc::new(SqliteDownloadCatalog::new(pool.clone())),
        Arc::new(WindowsDownloadPlatform::new(storage)),
        reconciliation_scheduler,
    ));
    let origin = origin_router_with_authenticator_and_file_mutations(
        &options.config_path,
        production_authenticator(&options.config)?,
        readiness.clone(),
        options.shutdown.clone(),
        OriginFileMutationServices {
            uploads: upload_service,
            files: file_service,
            downloads: download_source,
            mutations: mutation_source,
        },
    );
    let result = serve_bound(
        listeners,
        origin,
        health_router(readiness),
        rustls,
        options.on_started.take(),
        options.shutdown,
    )
    .await;
    reconciliation_task.abort();
    let _ = reconciliation_task.await;
    pool.close().await;
    result.map_err(|_| AppError::Listener)
}

pub(crate) fn sync_origin_trust_readiness(readiness: &Readiness, material: &TlsMaterial) {
    if material.pending_rotation().is_some() {
        readiness.block(ReadinessBlocker::OriginTrustUpdateRequired);
    } else {
        readiness.clear(ReadinessBlocker::OriginTrustUpdateRequired);
    }
}

pub async fn check_startup_gates(pool: &sqlx::SqlitePool) -> Result<StartupGates, AppError> {
    let pending_operations: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM operation WHERE state IN ('pending', 'fs_applied')",
    )
    .fetch_one(pool)
    .await
    .map_err(|_| AppError::Database)?;
    if pending_operations != 0 {
        return Err(AppError::Recovery);
    }
    Ok(StartupGates {
        recovery_complete: true,
        reconciliation_complete: true,
    })
}

fn apply_reconciliation_startup_gate(
    readiness: &Readiness,
    startup_gates: &StartupGates,
    has_queued_reconciliation: bool,
) {
    if startup_gates.reconciliation_complete && !has_queued_reconciliation {
        readiness.clear(ReadinessBlocker::ReconciliationRequired);
    }
}

pub async fn initialize_upload_recovery(
    pool: &sqlx::SqlitePool,
    staging: Arc<dyn UploadStagingStore>,
    readiness: &Readiness,
    now: time::OffsetDateTime,
) -> Result<UploadService, AppError> {
    let repository = Arc::new(SqliteUploadRepository::new(pool.clone()));
    let service = UploadService::new(repository, staging, UploadLimits::default());
    service
        .initialize(now)
        .await
        .map_err(|_| AppError::Recovery)?;
    readiness.clear(ReadinessBlocker::RecoveryRequired);
    Ok(service)
}

async fn serve_bound(
    listeners: BoundListeners,
    origin: Router,
    health: Router,
    tls: RustlsConfig,
    on_started: Option<StartedCallback>,
    shutdown: Shutdown,
) -> Result<(), ListenerError> {
    let handle = Handle::new();
    let origin_shutdown = shutdown.clone();
    let origin_handle = handle.clone();
    let shutdown_task = tokio::spawn(async move {
        let control = origin_shutdown.wait().await;
        origin_handle.graceful_shutdown(Some(shutdown_budget(control)));
    });
    let health_listener = tokio::net::TcpListener::from_std(listeners.health)
        .map_err(|_| ListenerError::ServeFailed)?;
    let health_shutdown = shutdown.clone();
    let health_deadline = shutdown.clone();
    let origin_server = axum_server::from_tcp_rustls(listeners.origin, tls)
        .map_err(|_| ListenerError::ServeFailed)?
        .handle(handle)
        .serve(origin.into_make_service());
    let health_server = async move {
        let graceful = axum::serve(health_listener, health)
            .with_graceful_shutdown(async move {
                let _ = health_shutdown.wait().await;
            })
            .into_future();
        tokio::pin!(graceful);
        tokio::select! {
            result = &mut graceful => result,
            control = health_deadline.wait() => {
                match tokio::time::timeout(shutdown_budget(control), &mut graceful).await {
                    Ok(result) => result,
                    Err(_) => Ok(()),
                }
            }
        }
    };
    if let Some(notify) = on_started {
        notify()?;
    }
    let result = tokio::try_join!(origin_server, health_server)
        .map(|_| ())
        .map_err(|_| ListenerError::ServeFailed);
    if shutdown.accepting_mutations() {
        shutdown.signal(ServiceControl::Stop);
    }
    let _ = shutdown_task.await;
    result
}

const fn shutdown_budget(control: ServiceControl) -> Duration {
    match control {
        ServiceControl::Stop => NORMAL_STOP_TARGET,
        ServiceControl::Preshutdown => PRESHUTDOWN_BUDGET,
    }
}

#[cfg(windows)]
fn compare_filename_ordinal(left: &str, right: &str) -> Ordering {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Globalization::{
        CSTR_EQUAL, CSTR_GREATER_THAN, CSTR_LESS_THAN, CompareStringOrdinal,
    };

    let left: Vec<u16> = std::ffi::OsStr::new(left).encode_wide().collect();
    let right: Vec<u16> = std::ffi::OsStr::new(right).encode_wide().collect();
    // SAFETY: both UTF-16 buffers remain live for their exact explicit lengths.
    let result = unsafe {
        CompareStringOrdinal(
            left.as_ptr(),
            left.len() as i32,
            right.as_ptr(),
            right.len() as i32,
            1,
        )
    };
    match result {
        CSTR_LESS_THAN => Ordering::Less,
        CSTR_EQUAL => Ordering::Equal,
        CSTR_GREATER_THAN => Ordering::Greater,
        _ => left.cmp(&right),
    }
}

#[cfg(not(windows))]
fn compare_filename_ordinal(left: &str, right: &str) -> Ordering {
    left.cmp(right)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn queued_startup_reconciliation_keeps_readiness_blocked() {
        let gates = StartupGates {
            recovery_complete: true,
            reconciliation_complete: true,
        };
        let queued = Readiness::all_blocked();
        apply_reconciliation_startup_gate(&queued, &gates, true);
        assert!(
            queued
                .blocker_codes()
                .contains(&ReadinessBlocker::ReconciliationRequired.code())
        );

        let empty = Readiness::all_blocked();
        apply_reconciliation_startup_gate(&empty, &gates, false);
        assert!(
            !empty
                .blocker_codes()
                .contains(&ReadinessBlocker::ReconciliationRequired.code())
        );
    }

    struct CountingFetcher(AtomicUsize);

    #[async_trait]
    impl JwksFetcher for CountingFetcher {
        async fn fetch(&self, _url: &Url) -> Result<JwksResponse, JwksFetchError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(JwksResponse::new(
                200,
                Cursor::new(br#"{"keys":[]}"#.to_vec()),
                None,
            ))
        }
    }

    #[tokio::test]
    async fn production_multi_audience_composition_uses_one_validator_and_fetch_path() {
        let fetcher = Arc::new(CountingFetcher(AtomicUsize::new(0)));
        let config = CellarConfig {
            external_origin: Url::parse("https://cellar.example.test").unwrap(),
            team_domain: Url::parse("https://team.cloudflareaccess.com").unwrap(),
            aud_tags: vec!["first-audience".into(), "later-audience".into()],
            bootstrap_owner_email: None,
            owner_subject: Some("owner-subject".into()),
            storage_root: PathBuf::from(r"C:\cellar-storage"),
            origin_port: 8443,
            health_port: 8081,
        };
        let authenticator = build_origin_authenticator(&config, fetcher.clone())
            .expect("multi-audience authenticator");

        let error = authenticator
            .validate(
                "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCIsImtpZCI6Im1pc3NpbmcifQ.e30.AA",
                OwnerMode::Enrolled {
                    owner_subject: "owner-subject",
                },
            )
            .await
            .expect_err("empty JWKS is rejected");

        assert_eq!(error.code(), "jwks_malformed");
        assert_eq!(fetcher.0.load(Ordering::SeqCst), 1);
    }
}
