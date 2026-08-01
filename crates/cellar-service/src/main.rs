use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cellar_config::load_config;
use cellar_service::app::{RunOptions, Shutdown};
use cellar_service::logging::{
    FatalEvent, JsonLogger, LogEvent, LogLevel, RotationPolicy, SanitizedContext, WindowsEventLog,
};
use cellar_service::tls::OriginTlsPaths;
#[cfg(not(windows))]
use cellar_windows::service::ServiceControl;
#[cfg(windows)]
use cellar_windows::service::ServiceError;

fn main() -> ExitCode {
    match platform_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => {
            let _ = WindowsEventLog::new("Cellar")
                .record(&FatalEvent::new(code, SanitizedContext::new()));
            eprintln!("{code}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(windows)]
fn platform_main() -> Result<(), &'static str> {
    cellar_windows::service::run_service_host(|receiver| {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|_| ServiceError::HostFailed)?;
        let shutdown = Shutdown::new();
        let control = shutdown.clone();
        std::thread::spawn(move || {
            if let Ok(signal) = receiver.recv() {
                control.signal(signal);
            }
        });
        runtime
            .block_on(run_main(shutdown))
            .map_err(|_| ServiceError::HostFailed)
    })
    .map_err(|error| error.code())
}

#[cfg(not(windows))]
fn platform_main() -> Result<(), &'static str> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|_| "service_host_failed")?;
    let shutdown = Shutdown::new();
    let control = shutdown.clone();
    runtime.spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            control.signal(ServiceControl::Stop);
        }
    });
    runtime.block_on(run_main(shutdown))
}

async fn run_main(shutdown: Shutdown) -> Result<(), &'static str> {
    let state_directory = program_data_directory()?.join("Cellar");
    let config_path = state_directory.join("config.toml");
    let mut logger = JsonLogger::new(&state_directory.join("logs"), RotationPolicy::default())
        .map_err(|_| "startup_logging_failed")?;
    logger
        .write(&LogEvent::now(LogLevel::Info, "service_starting"))
        .map_err(|_| "startup_logging_failed")?;

    let persisted = match load_config(&config_path) {
        Ok(config) => config,
        Err(_) => return fatal(&mut logger, "startup_configuration_failed"),
    };
    let result = cellar_service::app::run(RunOptions {
        config: persisted.config,
        config_path,
        database_path: state_directory.join("cellar.db"),
        tls_paths: tls_paths(&state_directory),
        startup_gates: None,
        on_started: service_started_callback(),
        shutdown,
    })
    .await;
    match result {
        Ok(()) => {
            logger
                .write(&LogEvent::now(LogLevel::Info, "service_stopped"))
                .map_err(|_| "startup_logging_failed")?;
            Ok(())
        }
        Err(error) => fatal(&mut logger, error.code()),
    }
}

fn fatal(logger: &mut JsonLogger, code: &'static str) -> Result<(), &'static str> {
    let context = SanitizedContext::new();
    let _ = logger.write(&LogEvent::now(LogLevel::Error, code).with_context(context.clone()));
    let _ = WindowsEventLog::new("Cellar").record(&FatalEvent::new(code, context));
    Err(code)
}

fn tls_paths(state_directory: &Path) -> OriginTlsPaths {
    let directory = state_directory.join("tls");
    OriginTlsPaths {
        ca_cert: directory.join("origin-ca.pem"),
        ca_key: directory.join("origin-ca.key.pem"),
        leaf_cert: directory.join("origin-leaf.pem"),
        leaf_key: directory.join("origin-leaf.key.pem"),
    }
}

#[cfg(windows)]
fn service_started_callback() -> Option<cellar_service::app::StartedCallback> {
    Some(Box::new(|| {
        cellar_windows::service::mark_service_running()
            .map_err(|_| cellar_service::app::ListenerError::ServeFailed)
    }))
}

#[cfg(not(windows))]
fn service_started_callback() -> Option<cellar_service::app::StartedCallback> {
    None
}

#[cfg(windows)]
fn program_data_directory() -> Result<PathBuf, &'static str> {
    use std::os::windows::ffi::OsStringExt;
    use std::ptr;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_ProgramData, SHGetKnownFolderPath};

    let mut raw = ptr::null_mut();
    // SAFETY: `raw` is a writable out-pointer; the shell allocates a
    // NUL-terminated path that is released once with CoTaskMemFree below.
    if unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramData, 0, ptr::null_mut(), &mut raw) } < 0
        || raw.is_null()
    {
        return Err("startup_program_data_unavailable");
    }
    let mut length = 0;
    // SAFETY: a successful SHGetKnownFolderPath returns a valid
    // NUL-terminated UTF-16 allocation.
    while unsafe { *raw.add(length) } != 0 {
        length += 1;
    }
    // SAFETY: the allocation contains exactly `length` initialized code units.
    let path = PathBuf::from(std::ffi::OsString::from_wide(unsafe {
        std::slice::from_raw_parts(raw, length)
    }));
    // SAFETY: `raw` is the allocation returned by SHGetKnownFolderPath and is freed once.
    unsafe { CoTaskMemFree(raw.cast()) };
    if path.is_absolute() {
        Ok(path)
    } else {
        Err("startup_program_data_unavailable")
    }
}

#[cfg(not(windows))]
fn program_data_directory() -> Result<PathBuf, &'static str> {
    Err("startup_platform_unsupported")
}
