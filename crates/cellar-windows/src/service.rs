use std::ffi::OsStr;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::mpsc::Receiver;
use std::time::Duration;

pub const NORMAL_STOP_TARGET: Duration = Duration::from_secs(60);
pub const PRESHUTDOWN_BUDGET: Duration = Duration::from_secs(180);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceControl {
    Stop,
    Preshutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryMode {
    Service,
    Console,
}

pub fn select_entry_mode<I, S>(arguments: I) -> Result<EntryMode, ServiceError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut arguments = arguments.into_iter();
    let mode = match arguments.next() {
        None => EntryMode::Service,
        Some(argument) if argument.as_ref() == "--service" => EntryMode::Service,
        Some(argument) if argument.as_ref() == "--console" => EntryMode::Console,
        Some(_) => return Err(ServiceError::InvalidMode),
    };
    if arguments.next().is_some() {
        return Err(ServiceError::InvalidMode);
    }
    Ok(mode)
}

pub const ACCEPT_STOP: u32 = 0x0000_0001;
pub const ACCEPT_PRESHUTDOWN: u32 = 0x0000_0100;

#[must_use]
pub const fn accepted_controls() -> u32 {
    ACCEPT_STOP | ACCEPT_PRESHUTDOWN
}

#[must_use]
pub const fn control_from_raw(control: u32) -> Option<ServiceControl> {
    match control {
        1 => Some(ServiceControl::Stop),
        15 => Some(ServiceControl::Preshutdown),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecoveryIntent {
    pub restart_delay: Duration,
    pub maximum_restarts: u8,
}

impl Default for RecoveryIntent {
    fn default() -> Self {
        Self {
            restart_delay: Duration::from_secs(30),
            maximum_restarts: 3,
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum ServiceError {
    UnsupportedPlatform,
    ConfigurationFailed,
    DispatcherFailed,
    HostFailed,
    InvalidMode,
    StatusFailed,
}

impl ServiceError {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::UnsupportedPlatform => "service_host_unsupported",
            Self::ConfigurationFailed => "service_recovery_configuration_failed",
            Self::DispatcherFailed => "service_dispatcher_failed",
            Self::HostFailed => "service_host_failed",
            Self::InvalidMode => "service_mode_invalid",
            Self::StatusFailed => "service_status_failed",
        }
    }
}

impl fmt::Debug for ServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for ServiceError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScmStatus {
    StartPending,
    Running,
    StopPending(ServiceControl),
    Stopped { failed: bool },
}

trait StatusReporter {
    fn report(&self, status: ScmStatus) -> Result<(), ServiceError>;
}

fn record_status(
    reporter: &impl StatusReporter,
    status_failed: &AtomicBool,
    status: ScmStatus,
) -> Result<(), ServiceError> {
    reporter.report(status).map_err(|_| {
        status_failed.store(true, AtomicOrdering::Release);
        ServiceError::StatusFailed
    })
}

fn finish_status(
    reporter: &impl StatusReporter,
    status_failed: &AtomicBool,
    runner_result: Result<(), ServiceError>,
) -> Result<(), ServiceError> {
    let failed = runner_result.is_err() || status_failed.load(AtomicOrdering::Acquire);
    let stopped_result = record_status(reporter, status_failed, ScmStatus::Stopped { failed });
    if stopped_result.is_err() || status_failed.load(AtomicOrdering::Acquire) {
        Err(ServiceError::StatusFailed)
    } else {
        runner_result
    }
}

fn run_guarded(
    reporter: &impl StatusReporter,
    status_failed: &AtomicBool,
    runner: impl FnOnce() -> Result<(), ServiceError>,
    cleanup: impl FnOnce(),
) -> Result<(), ServiceError> {
    let runner_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(runner))
        .unwrap_or(Err(ServiceError::HostFailed));
    cleanup();
    finish_status(reporter, status_failed, runner_result)
}

#[cfg(not(windows))]
pub fn run_service_host(
    _runner: impl FnOnce(Receiver<ServiceControl>) -> Result<(), ServiceError> + Send + 'static,
) -> Result<(), ServiceError> {
    Err(ServiceError::UnsupportedPlatform)
}

#[cfg(windows)]
pub fn run_service_host(
    runner: impl FnOnce(Receiver<ServiceControl>) -> Result<(), ServiceError> + Send + 'static,
) -> Result<(), ServiceError> {
    windows_host::run(Box::new(runner))
}

#[cfg(not(windows))]
pub fn mark_service_running() -> Result<(), ServiceError> {
    Err(ServiceError::UnsupportedPlatform)
}

#[cfg(windows)]
pub fn mark_service_running() -> Result<(), ServiceError> {
    windows_host::mark_running()
}

#[cfg(not(windows))]
pub fn configure_recovery_intent(_service_handle: isize) -> Result<(), ServiceError> {
    Err(ServiceError::UnsupportedPlatform)
}

#[cfg(windows)]
pub fn configure_recovery_intent(service_handle: isize) -> Result<(), ServiceError> {
    use std::ptr;
    use windows_sys::Win32::System::Services::{
        ChangeServiceConfig2W, SC_ACTION, SC_ACTION_NONE, SC_ACTION_RESTART,
        SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONFIG_PRESHUTDOWN_INFO, SERVICE_FAILURE_ACTIONSW,
        SERVICE_PRESHUTDOWN_INFO,
    };

    let intent = RecoveryIntent::default();
    let delay = u32::try_from(intent.restart_delay.as_millis())
        .map_err(|_| ServiceError::ConfigurationFailed)?;
    let mut actions = [
        SC_ACTION {
            Type: SC_ACTION_RESTART,
            Delay: delay,
        },
        SC_ACTION {
            Type: SC_ACTION_RESTART,
            Delay: delay,
        },
        SC_ACTION {
            Type: SC_ACTION_RESTART,
            Delay: delay,
        },
        SC_ACTION {
            Type: SC_ACTION_NONE,
            Delay: 0,
        },
    ];
    let mut failures = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: 24 * 60 * 60,
        lpRebootMsg: ptr::null_mut(),
        lpCommand: ptr::null_mut(),
        cActions: actions.len() as u32,
        lpsaActions: actions.as_mut_ptr(),
    };
    // SAFETY: the caller supplies an SCM service handle, and `failures` plus
    // its action array remain live for the synchronous configuration call.
    if unsafe {
        ChangeServiceConfig2W(
            service_handle as *mut core::ffi::c_void,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            (&mut failures as *mut SERVICE_FAILURE_ACTIONSW).cast(),
        )
    } == 0
    {
        return Err(ServiceError::ConfigurationFailed);
    }
    let mut preshutdown = SERVICE_PRESHUTDOWN_INFO {
        dwPreshutdownTimeout: PRESHUTDOWN_BUDGET.as_millis() as u32,
    };
    // SAFETY: the service handle is unchanged and `preshutdown` is a live,
    // correctly typed configuration structure.
    if unsafe {
        ChangeServiceConfig2W(
            service_handle as *mut core::ffi::c_void,
            SERVICE_CONFIG_PRESHUTDOWN_INFO,
            (&mut preshutdown as *mut SERVICE_PRESHUTDOWN_INFO).cast(),
        )
    } == 0
    {
        return Err(ServiceError::ConfigurationFailed);
    }
    Ok(())
}

#[cfg(windows)]
mod windows_host {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::ptr;
    use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::sync::{Mutex, OnceLock};

    use windows_sys::Win32::Foundation::ERROR_SERVICE_SPECIFIC_ERROR;
    use windows_sys::Win32::System::Services::{
        RegisterServiceCtrlHandlerExW, SERVICE_ACCEPT_PRESHUTDOWN, SERVICE_ACCEPT_STOP,
        SERVICE_CONTROL_PRESHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_START_PENDING,
        SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_STOP_PENDING, SERVICE_STOPPED,
        SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS, SetServiceStatus,
        StartServiceCtrlDispatcherW,
    };

    use super::{
        PRESHUTDOWN_BUDGET, ScmStatus, ServiceControl, ServiceError, StatusReporter, record_status,
    };

    type Runner = Box<dyn FnOnce(Receiver<ServiceControl>) -> Result<(), ServiceError> + Send>;

    static RUNNER: OnceLock<Mutex<Option<Runner>>> = OnceLock::new();
    static CONTROL: OnceLock<Mutex<Option<Sender<ServiceControl>>>> = OnceLock::new();
    static RESULT: OnceLock<Mutex<Option<Result<(), ServiceError>>>> = OnceLock::new();
    static STATUS: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(ptr::null_mut());
    static STATUS_FAILED: AtomicBool = AtomicBool::new(false);

    struct WindowsStatusReporter(SERVICE_STATUS_HANDLE);

    impl StatusReporter for WindowsStatusReporter {
        fn report(&self, status: ScmStatus) -> Result<(), ServiceError> {
            report_windows_status(self.0, status)
        }
    }

    pub(super) fn run(runner: Runner) -> Result<(), ServiceError> {
        STATUS_FAILED.store(false, Ordering::Release);
        let slot = RUNNER.get_or_init(|| Mutex::new(None));
        let mut guard = slot.lock().map_err(|_| ServiceError::DispatcherFailed)?;
        if guard.is_some() {
            return Err(ServiceError::DispatcherFailed);
        }
        *guard = Some(runner);
        drop(guard);

        let mut service_name: Vec<u16> = "Cellar".encode_utf16().chain(Some(0)).collect();
        let table = [
            SERVICE_TABLE_ENTRYW {
                lpServiceName: service_name.as_mut_ptr(),
                lpServiceProc: Some(service_main),
            },
            SERVICE_TABLE_ENTRYW {
                lpServiceName: ptr::null_mut(),
                lpServiceProc: None,
            },
        ];
        // SAFETY: `table` is terminated by a null entry and its service-name
        // buffer remains live while the dispatcher blocks synchronously.
        if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
            RUNNER
                .get()
                .and_then(|slot| slot.lock().ok())
                .and_then(|mut runner| runner.take());
            return Err(ServiceError::DispatcherFailed);
        }
        RESULT
            .get()
            .and_then(|slot| slot.lock().ok())
            .and_then(|mut result| result.take())
            .unwrap_or(Err(ServiceError::HostFailed))
    }

    unsafe extern "system" fn service_main(_argc: u32, _argv: *mut *mut u16) {
        let _ = catch_unwind(AssertUnwindSafe(service_main_inner));
    }

    fn service_main_inner() {
        let name: Vec<u16> = "Cellar".encode_utf16().chain(Some(0)).collect();
        // SAFETY: `name` is NUL-terminated and the handler has the required
        // system ABI and remains valid for the service lifetime.
        let handle = unsafe {
            RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(control_handler), ptr::null())
        };
        if handle.is_null() {
            return;
        }
        STATUS.store(handle, Ordering::Release);
        let reporter = WindowsStatusReporter(handle);
        if record_status(&reporter, &STATUS_FAILED, ScmStatus::StartPending).is_err() {
            store_result(Err(ServiceError::StatusFailed));
            STATUS.store(ptr::null_mut(), Ordering::Release);
            return;
        }
        let (sender, receiver) = channel();
        *CONTROL
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sender);
        let runner = RUNNER
            .get()
            .and_then(|slot| slot.lock().ok())
            .and_then(|mut runner| runner.take())
            .ok_or(ServiceError::HostFailed);
        let result = super::run_guarded(
            &reporter,
            &STATUS_FAILED,
            move || runner.and_then(|runner| runner(receiver)),
            || {
                CONTROL
                    .get()
                    .and_then(|slot| slot.lock().ok())
                    .and_then(|mut sender| sender.take());
            },
        );
        store_result(result);
        STATUS.store(ptr::null_mut(), Ordering::Release);
    }

    pub(super) fn mark_running() -> Result<(), ServiceError> {
        let handle = STATUS.load(Ordering::Acquire);
        if handle.is_null() {
            return Err(ServiceError::HostFailed);
        }
        record_status(
            &WindowsStatusReporter(handle),
            &STATUS_FAILED,
            ScmStatus::Running,
        )
    }

    fn store_result(result: Result<(), ServiceError>) {
        *RESULT
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
    }

    unsafe extern "system" fn control_handler(
        control: u32,
        _event_type: u32,
        _event_data: *mut core::ffi::c_void,
        _context: *mut core::ffi::c_void,
    ) -> u32 {
        let signal = match control {
            SERVICE_CONTROL_STOP => ServiceControl::Stop,
            SERVICE_CONTROL_PRESHUTDOWN => ServiceControl::Preshutdown,
            _ => return 120,
        };
        let handle = STATUS.load(Ordering::Acquire);
        if !handle.is_null() {
            let _ = record_status(
                &WindowsStatusReporter(handle),
                &STATUS_FAILED,
                ScmStatus::StopPending(signal),
            );
        }
        if let Some(sender) = CONTROL
            .get()
            .and_then(|slot| slot.lock().ok())
            .and_then(|sender| sender.clone())
        {
            let _ = sender.send(signal);
        }
        0
    }

    fn report_windows_status(
        handle: SERVICE_STATUS_HANDLE,
        transition: ScmStatus,
    ) -> Result<(), ServiceError> {
        let (state, accepted, wait_hint, failed) = match transition {
            ScmStatus::StartPending => (
                SERVICE_START_PENDING,
                0,
                // Installed-SCM acceptance must verify startup completes
                // within this static wait hint; no checkpoint timer is owned
                // by this foundation layer.
                PRESHUTDOWN_BUDGET.as_millis() as u32,
                false,
            ),
            ScmStatus::Running => (
                SERVICE_RUNNING,
                SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_PRESHUTDOWN,
                0,
                false,
            ),
            ScmStatus::StopPending(control) => (
                SERVICE_STOP_PENDING,
                0,
                match control {
                    ServiceControl::Stop => 60_000,
                    ServiceControl::Preshutdown => PRESHUTDOWN_BUDGET.as_millis() as u32,
                },
                false,
            ),
            ScmStatus::Stopped { failed } => (SERVICE_STOPPED, 0, 0, failed),
        };
        let status = SERVICE_STATUS {
            dwServiceType: SERVICE_WIN32_OWN_PROCESS,
            dwCurrentState: state,
            dwControlsAccepted: accepted,
            dwWin32ExitCode: if failed {
                ERROR_SERVICE_SPECIFIC_ERROR
            } else {
                0
            },
            dwServiceSpecificExitCode: u32::from(failed),
            dwCheckPoint: u32::from(matches!(
                state,
                SERVICE_START_PENDING | SERVICE_STOP_PENDING
            )),
            dwWaitHint: wait_hint,
        };
        // SAFETY: `handle` is registered with SCM and `status` is a live,
        // fully initialized structure for this synchronous report.
        if unsafe { SetServiceStatus(handle, &status) } == 0 {
            Err(ServiceError::StatusFailed)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    struct FakeReporter {
        fail_on: ScmStatus,
        seen: Mutex<Vec<ScmStatus>>,
    }

    impl StatusReporter for FakeReporter {
        fn report(&self, status: ScmStatus) -> Result<(), ServiceError> {
            self.seen.lock().unwrap().push(status);
            if status == self.fail_on {
                Err(ServiceError::HostFailed)
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn reporter_failures_are_recorded_for_every_transition() {
        for status in [
            ScmStatus::StartPending,
            ScmStatus::Running,
            ScmStatus::StopPending(ServiceControl::Stop),
            ScmStatus::StopPending(ServiceControl::Preshutdown),
            ScmStatus::Stopped { failed: false },
        ] {
            let reporter = FakeReporter {
                fail_on: status,
                seen: Mutex::new(Vec::new()),
            };
            let failed = AtomicBool::new(false);
            assert_eq!(
                record_status(&reporter, &failed, status)
                    .unwrap_err()
                    .code(),
                "service_status_failed"
            );
            assert!(failed.load(std::sync::atomic::Ordering::Acquire));
        }
    }

    #[test]
    fn final_stopped_failure_overrides_a_successful_runner() {
        let reporter = FakeReporter {
            fail_on: ScmStatus::Stopped { failed: false },
            seen: Mutex::new(Vec::new()),
        };
        let failed = AtomicBool::new(false);
        assert_eq!(
            finish_status(&reporter, &failed, Ok(()))
                .unwrap_err()
                .code(),
            "service_status_failed"
        );
    }

    #[test]
    fn earlier_handler_status_failure_propagates_at_final_stop() {
        let reporter = FakeReporter {
            fail_on: ScmStatus::StopPending(ServiceControl::Stop),
            seen: Mutex::new(Vec::new()),
        };
        let failed = AtomicBool::new(false);
        let _ = record_status(
            &reporter,
            &failed,
            ScmStatus::StopPending(ServiceControl::Stop),
        );
        assert_eq!(
            finish_status(&reporter, &failed, Ok(()))
                .unwrap_err()
                .code(),
            "service_status_failed"
        );
    }

    #[test]
    fn runner_panic_cleans_up_and_attempts_failed_stopped_status() {
        let reporter = FakeReporter {
            fail_on: ScmStatus::StartPending,
            seen: Mutex::new(Vec::new()),
        };
        let failed = AtomicBool::new(false);
        let cleaned_up = AtomicBool::new(false);

        let result = run_guarded(
            &reporter,
            &failed,
            || -> Result<(), ServiceError> { panic!("runner panic") },
            || cleaned_up.store(true, Ordering::Release),
        );

        assert_eq!(result.unwrap_err().code(), "service_host_failed");
        assert!(cleaned_up.load(Ordering::Acquire));
        assert_eq!(
            *reporter.seen.lock().unwrap(),
            [ScmStatus::Stopped { failed: true }]
        );
    }
}
