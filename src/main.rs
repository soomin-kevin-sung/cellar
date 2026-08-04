use std::{
    ffi::OsString,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    process::ExitCode,
    sync::Arc,
};

use axum::Router;
use cellar::{
    app::build_cellar_app,
    auth::{AccessVerifier, CloudflareAccessVerifier},
    config::Config,
    db::Database,
    projects::audit_orphan_project_directories,
    storage::Storage,
    uploads::UploadService,
};
use thiserror::Error;
use tokio::net::TcpListener;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

type StartupFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, StartupError>> + Send + 'a>>;

#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
#[error("Cellar startup failed ({code})")]
struct StartupError {
    code: &'static str,
}

impl StartupError {
    const fn new(code: &'static str) -> Self {
        Self { code }
    }

    const fn code(&self) -> &'static str {
        self.code
    }
}

trait StartupSteps {
    type Config;
    type Storage;
    type Database;
    type Uploads;
    type Verifier;
    type App;
    type Listener;

    fn config_path(&mut self) -> Result<PathBuf, StartupError>;
    fn load_config(&mut self, path: &Path) -> Result<Self::Config, StartupError>;
    fn init_logging(&mut self) -> Result<(), StartupError>;
    fn init_storage<'a>(&'a mut self, config: &'a Self::Config)
    -> StartupFuture<'a, Self::Storage>;
    fn open_database<'a>(
        &'a mut self,
        config: &'a Self::Config,
    ) -> StartupFuture<'a, Self::Database>;
    fn audit_projects<'a>(
        &'a mut self,
        database: &'a Self::Database,
        storage: &'a Self::Storage,
    ) -> StartupFuture<'a, ()>;
    fn construct_uploads(
        &mut self,
        database: &Self::Database,
        storage: &Self::Storage,
    ) -> Result<Self::Uploads, StartupError>;
    fn recover_uploads<'a>(&'a mut self, uploads: &'a Self::Uploads) -> StartupFuture<'a, ()>;
    fn construct_verifier(&mut self, config: &Self::Config)
    -> Result<Self::Verifier, StartupError>;
    fn build_router(
        &mut self,
        config: &Self::Config,
        database: &Self::Database,
        storage: &Self::Storage,
        verifier: &Self::Verifier,
    ) -> Result<Self::App, StartupError>;
    fn bind<'a>(&'a mut self, config: &'a Self::Config) -> StartupFuture<'a, Self::Listener>;
    fn serve<'a>(&'a mut self, listener: Self::Listener, app: Self::App) -> StartupFuture<'a, ()>;
    fn close_database<'a>(&'a mut self, database: &'a Self::Database) -> StartupFuture<'a, ()>;
}

async fn run_startup<S: StartupSteps>(steps: &mut S) -> Result<(), StartupError> {
    let config_path = steps.config_path()?;
    let config = steps.load_config(&config_path)?;
    steps.init_logging()?;
    let storage = steps.init_storage(&config).await?;
    let database = steps.open_database(&config).await?;
    steps.audit_projects(&database, &storage).await?;
    let uploads = steps.construct_uploads(&database, &storage)?;
    steps.recover_uploads(&uploads).await?;
    let verifier = steps.construct_verifier(&config)?;
    let app = steps.build_router(&config, &database, &storage, &verifier)?;
    let listener = steps.bind(&config).await?;
    let serve_result = steps.serve(listener, app).await;
    let close_result = steps.close_database(&database).await;
    serve_result?;
    close_result
}

fn required_config_path(value: Option<OsString>) -> Result<PathBuf, StartupError> {
    let value = value.ok_or_else(|| StartupError::new("config_path_missing"))?;
    if value.is_empty() {
        return Err(StartupError::new("config_path_missing"));
    }
    Ok(PathBuf::from(value))
}

struct RuntimeStartup;

impl StartupSteps for RuntimeStartup {
    type Config = Config;
    type Storage = Arc<Storage>;
    type Database = Arc<Database>;
    type Uploads = UploadService;
    type Verifier = Arc<dyn AccessVerifier>;
    type App = Router;
    type Listener = TcpListener;

    fn config_path(&mut self) -> Result<PathBuf, StartupError> {
        required_config_path(std::env::var_os("CELLAR_CONFIG"))
    }

    fn load_config(&mut self, path: &Path) -> Result<Self::Config, StartupError> {
        Config::load(path).map_err(|_| StartupError::new("config_invalid"))
    }

    fn init_logging(&mut self) -> Result<(), StartupError> {
        let filter =
            EnvFilter::try_from_env("RUST_LOG").unwrap_or_else(|_| EnvFilter::new("cellar=info"));
        tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer().json())
            .try_init()
            .map_err(|_| StartupError::new("logging_init_failed"))
    }

    fn init_storage<'a>(
        &'a mut self,
        config: &'a Self::Config,
    ) -> StartupFuture<'a, Self::Storage> {
        Box::pin(async move {
            let storage = Arc::new(
                Storage::new(config.data_root().to_path_buf())
                    .map_err(|_| StartupError::new("storage_init_failed"))?,
            );
            storage
                .initialize()
                .await
                .map_err(|_| StartupError::new("storage_init_failed"))?;
            Ok(storage)
        })
    }

    fn open_database<'a>(
        &'a mut self,
        config: &'a Self::Config,
    ) -> StartupFuture<'a, Self::Database> {
        Box::pin(async move {
            Database::open(config.database_path())
                .await
                .map(Arc::new)
                .map_err(|_| StartupError::new("database_init_failed"))
        })
    }

    fn construct_uploads(
        &mut self,
        database: &Self::Database,
        storage: &Self::Storage,
    ) -> Result<Self::Uploads, StartupError> {
        Ok(UploadService::new(database.clone(), storage.clone()))
    }

    fn audit_projects<'a>(
        &'a mut self,
        database: &'a Self::Database,
        storage: &'a Self::Storage,
    ) -> StartupFuture<'a, ()> {
        Box::pin(async move {
            audit_orphan_project_directories(database.as_ref(), storage.as_ref())
                .await
                .map_err(|_| StartupError::new("project_audit_failed"))
        })
    }

    fn recover_uploads<'a>(&'a mut self, uploads: &'a Self::Uploads) -> StartupFuture<'a, ()> {
        Box::pin(async move {
            uploads
                .recover_uploads()
                .await
                .map_err(|_| StartupError::new("upload_recovery_failed"))
        })
    }

    fn construct_verifier(
        &mut self,
        config: &Self::Config,
    ) -> Result<Self::Verifier, StartupError> {
        CloudflareAccessVerifier::new(config.access())
            .map(|verifier| Arc::new(verifier) as Arc<dyn AccessVerifier>)
            .map_err(|_| StartupError::new("access_init_failed"))
    }

    fn build_router(
        &mut self,
        config: &Self::Config,
        database: &Self::Database,
        storage: &Self::Storage,
        verifier: &Self::Verifier,
    ) -> Result<Self::App, StartupError> {
        build_cellar_app(
            database.clone(),
            storage.clone(),
            verifier.clone(),
            config.external_origin(),
        )
        .map_err(|_| StartupError::new("web_assets_missing"))
    }

    fn bind<'a>(&'a mut self, config: &'a Self::Config) -> StartupFuture<'a, Self::Listener> {
        Box::pin(async move {
            TcpListener::bind(config.bind())
                .await
                .map_err(|_| StartupError::new("bind_failed"))
        })
    }

    fn serve<'a>(&'a mut self, listener: Self::Listener, app: Self::App) -> StartupFuture<'a, ()> {
        Box::pin(async move {
            serve_until_shutdown(listener, app, shutdown_signal())
                .await
                .map_err(|_| StartupError::new("serve_failed"))
        })
    }

    fn close_database<'a>(&'a mut self, database: &'a Self::Database) -> StartupFuture<'a, ()> {
        Box::pin(async move {
            database.as_ref().clone().close().await;
            Ok(())
        })
    }
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlatformShutdownSignal {
    CtrlC,
    CtrlBreak,
    ConsoleClose,
    SystemShutdown,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlatformShutdownSignal {
    CtrlC,
    Terminate,
}

#[cfg(windows)]
const PLATFORM_SHUTDOWN_SIGNALS: &[PlatformShutdownSignal] = &[
    PlatformShutdownSignal::CtrlC,
    PlatformShutdownSignal::CtrlBreak,
    PlatformShutdownSignal::ConsoleClose,
    PlatformShutdownSignal::SystemShutdown,
];

#[cfg(unix)]
const PLATFORM_SHUTDOWN_SIGNALS: &[PlatformShutdownSignal] = &[
    PlatformShutdownSignal::CtrlC,
    PlatformShutdownSignal::Terminate,
];

impl PlatformShutdownSignal {
    const fn name(self) -> &'static str {
        match self {
            Self::CtrlC => "ctrl_c",
            #[cfg(windows)]
            Self::CtrlBreak => "ctrl_break",
            #[cfg(windows)]
            Self::ConsoleClose => "console_close",
            #[cfg(windows)]
            Self::SystemShutdown => "system_shutdown",
            #[cfg(unix)]
            Self::Terminate => "terminate",
        }
    }
}

struct RegisteredShutdownSignal {
    kind: &'static str,
    wait: Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
}

impl RegisteredShutdownSignal {
    #[cfg(test)]
    fn for_test<F>(kind: &'static str, wait: F) -> Self
    where
        F: Future<Output = ()> + Send + 'static,
    {
        Self {
            kind,
            wait: Box::pin(wait),
        }
    }
}

fn register_shutdown_signal<F>(
    signals: &mut Vec<RegisteredShutdownSignal>,
    kind: &'static str,
    registration: std::io::Result<F>,
) where
    F: Future<Output = Option<()>> + Send + 'static,
{
    match registration {
        Ok(wait) => signals.push(RegisteredShutdownSignal {
            kind,
            wait: Box::pin(async move {
                if wait.await.is_none() {
                    tracing::warn!(
                        signal = kind,
                        reason = "listener_closed",
                        "shutdown signal listener stopped"
                    );
                    std::future::pending::<()>().await;
                }
            }),
        }),
        Err(_) => tracing::warn!(
            signal = kind,
            reason = "registration_failed",
            "shutdown signal listener unavailable"
        ),
    }
}

#[cfg(windows)]
fn platform_shutdown_signals() -> Vec<RegisteredShutdownSignal> {
    use tokio::signal::windows;

    let mut signals = Vec::with_capacity(PLATFORM_SHUTDOWN_SIGNALS.len());
    for kind in PLATFORM_SHUTDOWN_SIGNALS {
        match kind {
            PlatformShutdownSignal::CtrlC => register_shutdown_signal(
                &mut signals,
                kind.name(),
                windows::ctrl_c().map(|mut signal| async move { signal.recv().await }),
            ),
            PlatformShutdownSignal::CtrlBreak => register_shutdown_signal(
                &mut signals,
                kind.name(),
                windows::ctrl_break().map(|mut signal| async move { signal.recv().await }),
            ),
            PlatformShutdownSignal::ConsoleClose => register_shutdown_signal(
                &mut signals,
                kind.name(),
                windows::ctrl_close().map(|mut signal| async move { signal.recv().await }),
            ),
            PlatformShutdownSignal::SystemShutdown => register_shutdown_signal(
                &mut signals,
                kind.name(),
                windows::ctrl_shutdown().map(|mut signal| async move { signal.recv().await }),
            ),
        }
    }
    signals
}

#[cfg(unix)]
fn platform_shutdown_signals() -> Vec<RegisteredShutdownSignal> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut signals = Vec::with_capacity(PLATFORM_SHUTDOWN_SIGNALS.len());
    for kind in PLATFORM_SHUTDOWN_SIGNALS {
        let signal_kind = match kind {
            PlatformShutdownSignal::CtrlC => SignalKind::interrupt(),
            PlatformShutdownSignal::Terminate => SignalKind::terminate(),
        };
        register_shutdown_signal(
            &mut signals,
            kind.name(),
            signal(signal_kind).map(|mut signal| async move { signal.recv().await }),
        );
    }
    signals
}

async fn wait_for_shutdown_signal(signals: Vec<RegisteredShutdownSignal>) {
    if signals.is_empty() {
        tracing::error!(
            reason = "no_signal_listeners",
            "graceful shutdown signals are unavailable"
        );
        std::future::pending::<()>().await;
        return;
    }

    let waits: Vec<Pin<Box<dyn Future<Output = &'static str> + Send>>> = signals
        .into_iter()
        .map(|signal| {
            Box::pin(async move {
                signal.wait.await;
                signal.kind
            }) as Pin<Box<dyn Future<Output = &'static str> + Send>>
        })
        .collect();
    let (kind, _, _) = futures_util::future::select_all(waits).await;
    tracing::info!(signal = kind, "graceful shutdown requested");
}

async fn shutdown_signal() {
    wait_for_shutdown_signal(platform_shutdown_signals()).await;
}

async fn serve_until_shutdown<S>(
    listener: TcpListener,
    app: Router,
    signal: S,
) -> std::io::Result<()>
where
    S: Future<Output = ()> + Send + 'static,
{
    axum::serve(listener, app)
        .with_graceful_shutdown(signal)
        .await
}

#[tokio::main]
async fn main() -> ExitCode {
    match run_startup(&mut RuntimeStartup).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!(
                "{{\"level\":\"ERROR\",\"message\":\"Cellar startup failed\",\"code\":\"{}\"}}",
                error.code()
            );
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, sync::Arc, time::Duration};

    use axum::{Router, routing::get};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        sync::{Notify, oneshot},
    };

    use super::{
        RegisteredShutdownSignal, StartupError, StartupFuture, StartupSteps,
        platform_shutdown_signals, register_shutdown_signal, required_config_path, run_startup,
        serve_until_shutdown, wait_for_shutdown_signal,
    };

    struct FakeSteps {
        events: Vec<&'static str>,
        fail_at: Option<&'static str>,
    }

    impl FakeSteps {
        fn new(fail_at: Option<&'static str>) -> Self {
            Self {
                events: Vec::new(),
                fail_at,
            }
        }

        fn record(&mut self, event: &'static str) -> Result<(), StartupError> {
            self.events.push(event);
            if self.fail_at == Some(event) {
                Err(StartupError::new("fixture_failure"))
            } else {
                Ok(())
            }
        }
    }

    impl StartupSteps for FakeSteps {
        type Config = ();
        type Storage = ();
        type Database = ();
        type Uploads = ();
        type Verifier = ();
        type App = ();
        type Listener = ();

        fn config_path(&mut self) -> Result<std::path::PathBuf, StartupError> {
            self.record("config_path")?;
            Ok("fixture.toml".into())
        }

        fn load_config(&mut self, _path: &std::path::Path) -> Result<Self::Config, StartupError> {
            self.record("config")
        }

        fn init_logging(&mut self) -> Result<(), StartupError> {
            self.record("logging")
        }

        fn init_storage<'a>(
            &'a mut self,
            _config: &'a Self::Config,
        ) -> StartupFuture<'a, Self::Storage> {
            Box::pin(async move { self.record("storage") })
        }

        fn open_database<'a>(
            &'a mut self,
            _config: &'a Self::Config,
        ) -> StartupFuture<'a, Self::Database> {
            Box::pin(async move { self.record("database") })
        }

        fn construct_uploads(
            &mut self,
            _database: &Self::Database,
            _storage: &Self::Storage,
        ) -> Result<Self::Uploads, StartupError> {
            self.record("uploads")
        }

        fn audit_projects<'a>(
            &'a mut self,
            _database: &'a Self::Database,
            _storage: &'a Self::Storage,
        ) -> StartupFuture<'a, ()> {
            Box::pin(async move { self.record("audit") })
        }

        fn recover_uploads<'a>(&'a mut self, _uploads: &'a Self::Uploads) -> StartupFuture<'a, ()> {
            Box::pin(async move { self.record("recovery") })
        }

        fn construct_verifier(
            &mut self,
            _config: &Self::Config,
        ) -> Result<Self::Verifier, StartupError> {
            self.record("verifier")
        }

        fn build_router(
            &mut self,
            _config: &Self::Config,
            _database: &Self::Database,
            _storage: &Self::Storage,
            _verifier: &Self::Verifier,
        ) -> Result<Self::App, StartupError> {
            self.record("router")
        }

        fn bind<'a>(&'a mut self, _config: &'a Self::Config) -> StartupFuture<'a, Self::Listener> {
            Box::pin(async move { self.record("bind") })
        }

        fn serve<'a>(
            &'a mut self,
            _listener: Self::Listener,
            _app: Self::App,
        ) -> StartupFuture<'a, ()> {
            Box::pin(async move { self.record("serve") })
        }

        fn close_database<'a>(
            &'a mut self,
            _database: &'a Self::Database,
        ) -> StartupFuture<'a, ()> {
            Box::pin(async move { self.record("close") })
        }
    }

    #[tokio::test]
    async fn startup_runs_project_audit_after_database_and_before_recovery_and_bind() {
        let mut steps = FakeSteps::new(None);
        run_startup(&mut steps).await.unwrap();
        assert_eq!(
            steps.events,
            [
                "config_path",
                "config",
                "logging",
                "storage",
                "database",
                "audit",
                "uploads",
                "recovery",
                "verifier",
                "router",
                "bind",
                "serve",
                "close",
            ]
        );
    }

    #[tokio::test]
    async fn startup_failure_short_circuits_every_later_step() {
        for (failure, expected) in [
            (
                "storage",
                vec!["config_path", "config", "logging", "storage"],
            ),
            (
                "recovery",
                vec![
                    "config_path",
                    "config",
                    "logging",
                    "storage",
                    "database",
                    "audit",
                    "uploads",
                    "recovery",
                ],
            ),
            (
                "audit",
                vec![
                    "config_path",
                    "config",
                    "logging",
                    "storage",
                    "database",
                    "audit",
                ],
            ),
            (
                "bind",
                vec![
                    "config_path",
                    "config",
                    "logging",
                    "storage",
                    "database",
                    "audit",
                    "uploads",
                    "recovery",
                    "verifier",
                    "router",
                    "bind",
                ],
            ),
        ] {
            let mut steps = FakeSteps::new(Some(failure));
            assert_eq!(
                run_startup(&mut steps).await.unwrap_err().code(),
                "fixture_failure"
            );
            assert_eq!(steps.events, expected, "{failure}");
        }
    }

    #[test]
    fn config_environment_value_is_required_nonempty_and_error_is_safe() {
        for value in [None, Some(OsString::new())] {
            let error = required_config_path(value).unwrap_err();
            assert_eq!(error.code(), "config_path_missing");
            assert_eq!(
                error.to_string(),
                "Cellar startup failed (config_path_missing)"
            );
        }
        assert_eq!(
            required_config_path(Some(OsString::from("C:/path with spaces/cellar.toml"))).unwrap(),
            std::path::PathBuf::from("C:/path with spaces/cellar.toml")
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_runtime_registers_every_supported_shutdown_signal() {
        let registered = platform_shutdown_signals();
        assert_eq!(
            registered
                .iter()
                .map(|signal| signal.kind)
                .collect::<Vec<_>>(),
            ["ctrl_c", "ctrl_break", "console_close", "system_shutdown"]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_runtime_registers_interrupt_and_terminate_signals() {
        let registered = platform_shutdown_signals();
        assert_eq!(
            registered
                .iter()
                .map(|signal| signal.kind)
                .collect::<Vec<_>>(),
            ["ctrl_c", "terminate"]
        );
    }

    #[tokio::test]
    async fn shutdown_aggregation_completes_when_any_registered_signal_fires() {
        let (first_tx, first_rx) = oneshot::channel::<()>();
        let (_second_tx, second_rx) = oneshot::channel::<()>();
        let signals = vec![
            RegisteredShutdownSignal::for_test("first", async move {
                let _ = first_rx.await;
            }),
            RegisteredShutdownSignal::for_test("second", async move {
                let _ = second_rx.await;
            }),
        ];

        first_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), wait_for_shutdown_signal(signals))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn failed_or_closed_signal_registration_never_busy_loops_or_stops_the_server() {
        let mut signals = Vec::new();
        register_shutdown_signal(
            &mut signals,
            "registration_failure",
            Err::<std::future::Pending<Option<()>>, _>(std::io::Error::other("fixture")),
        );
        register_shutdown_signal(
            &mut signals,
            "closed_listener",
            Ok(std::future::ready(None)),
        );

        assert_eq!(signals.len(), 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(25), wait_for_shutdown_signal(signals),)
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(
                Duration::from_millis(25),
                wait_for_shutdown_signal(Vec::new()),
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn graceful_shutdown_waits_for_an_in_flight_request_to_drain() {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let app = Router::new().route(
            "/slow",
            get({
                let started = started.clone();
                let release = release.clone();
                move || {
                    let started = started.clone();
                    let release = release.clone();
                    async move {
                        started.notify_one();
                        release.notified().await;
                        "drained"
                    }
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server = tokio::spawn(serve_until_shutdown(listener, app, async move {
            let _ = shutdown_rx.await;
        }));
        let mut client = TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"GET /slow HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), started.notified())
            .await
            .unwrap();
        shutdown_tx.send(()).unwrap();
        tokio::task::yield_now().await;
        assert!(!server.is_finished());
        release.notify_one();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(String::from_utf8(response).unwrap().contains("drained"));
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[test]
    fn development_launcher_has_only_the_expected_local_build_and_run_flow() {
        let script =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/run-dev.ps1"))
                .unwrap();
        for required in [
            "node_modules",
            "npm run build",
            "CELLAR_CONFIG",
            "cargo run",
            "$PSScriptRoot",
            "Test-Path",
        ] {
            assert!(script.contains(required), "missing {required}");
        }
        for forbidden in [
            "cloudflared",
            "New-Service",
            "Set-Service",
            "npm install",
            "npm ci",
        ] {
            assert!(!script.contains(forbidden), "forbidden {forbidden}");
        }
    }
}
