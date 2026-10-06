use std::{
    cell::Cell,
    ffi::{CString, c_char, c_void},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevel {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl From<LogLevel> for log::LevelFilter {
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::Off => Self::Off,
            LogLevel::Error => Self::Error,
            LogLevel::Warn => Self::Warn,
            LogLevel::Info => Self::Info,
            LogLevel::Debug => Self::Debug,
            LogLevel::Trace => Self::Trace,
        }
    }
}

impl From<log::LevelFilter> for LogLevel {
    fn from(level: log::LevelFilter) -> Self {
        match level {
            log::LevelFilter::Off => Self::Off,
            log::LevelFilter::Error => Self::Error,
            log::LevelFilter::Warn => Self::Warn,
            log::LevelFilter::Info => Self::Info,
            log::LevelFilter::Debug => Self::Debug,
            log::LevelFilter::Trace => Self::Trace,
        }
    }
}

#[derive(Clone, Copy)]
struct LogCallback {
    callback: unsafe extern "C" fn(LogLevel, *const c_char, *mut c_void),
    ctx: *mut c_void,
}

unsafe impl Send for LogCallback {}
unsafe impl Sync for LogCallback {}

impl LogCallback {
    unsafe fn call(self, level: LogLevel, message: *const c_char) {
        unsafe { (self.callback)(level, message, self.ctx) };
    }
}

static CALLBACK: Mutex<Option<LogCallback>> = Mutex::new(None);
static LOGGER_INSTALLED: AtomicBool = AtomicBool::new(false);
static LOGGER_INSTALL_LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    static IN_LOG_CALLBACK: Cell<bool> = const { Cell::new(false) };
}

struct CallbackReentrancyGuard;

impl CallbackReentrancyGuard {
    fn enter() -> Option<Self> {
        IN_LOG_CALLBACK.with(|in_callback| if in_callback.replace(true) { None } else { Some(Self) })
    }
}

impl Drop for CallbackReentrancyGuard {
    fn drop(&mut self) {
        IN_LOG_CALLBACK.with(|in_callback| in_callback.set(false));
    }
}

pub(crate) fn set_callback(
    set_logger: bool,
    callback: Option<unsafe extern "C" fn(LogLevel, *const c_char, *mut c_void)>,
    ctx: *mut c_void,
) {
    let mut registered = CALLBACK.lock().unwrap_or_else(|error| error.into_inner());
    *registered = callback.map(|callback| LogCallback { callback, ctx });
    drop(registered);

    if set_logger && install_logger() {
        log::set_max_level(log::LevelFilter::Trace);
    }
}

pub(crate) fn prepare_ffi_client(log_level: log::LevelFilter) {
    let callback_registered = CALLBACK.lock().unwrap_or_else(|error| error.into_inner()).is_some();
    let logger_available = if callback_registered {
        install_logger()
    } else {
        LOGGER_INSTALLED.load(Ordering::Acquire)
    };
    if logger_available {
        log::set_max_level(log_level);
    }
}

fn install_logger() -> bool {
    let _install_guard = LOGGER_INSTALL_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    if LOGGER_INSTALLED.load(Ordering::Acquire) {
        return true;
    }

    match log::set_boxed_logger(Box::<CallbackLogger>::default()) {
        Ok(()) => {
            LOGGER_INSTALLED.store(true, Ordering::Release);
            true
        }
        Err(error) => {
            log::warn!("failed to install AnyTLS callback logger: {error}");
            false
        }
    }
}

#[derive(Default)]
struct CallbackLogger;

impl CallbackLogger {
    fn should_forward(module_path: &str) -> bool {
        !["rustls", "tungstenite", "tokio_tungstenite"]
            .iter()
            .any(|module| module_path == *module || module_path.strip_prefix(module).is_some_and(|suffix| suffix.starts_with("::")))
    }
}

impl log::Log for CallbackLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Trace
    }

    fn log(&self, record: &log::Record) {
        let Some(_guard) = CallbackReentrancyGuard::enter() else {
            return;
        };
        if !self.enabled(record.metadata()) {
            return;
        }
        if !Self::should_forward(record.module_path().unwrap_or("")) {
            return;
        }
        let module_path = record.module_path().unwrap_or(record.target());

        let callback = *CALLBACK.lock().unwrap_or_else(|error| error.into_inner());
        let Some(callback) = callback else {
            return;
        };

        let message = format!("[{:<5} {}] - {}", record.level(), module_path, record.args()).replace('\0', "\\0");
        let Ok(message) = CString::new(message) else {
            return;
        };
        unsafe { callback.call(record.level().to_level_filter().into(), message.as_ptr()) };
    }

    fn flush(&self) {}
}

#[cfg(test)]
mod tests {
    use super::{CallbackLogger, CallbackReentrancyGuard, LogLevel};

    #[test]
    fn forwards_all_targets_except_noisy_tls_and_websocket_modules() {
        assert!(CallbackLogger::should_forward("anytls::client"));
        assert!(CallbackLogger::should_forward("tokio::runtime"));
        assert!(CallbackLogger::should_forward("rustls_extra::client"));
        assert!(!CallbackLogger::should_forward("rustls"));
        assert!(!CallbackLogger::should_forward("rustls::client"));
        assert!(!CallbackLogger::should_forward("tungstenite::handshake"));
        assert!(!CallbackLogger::should_forward("tokio_tungstenite::stream"));
    }

    #[test]
    fn callback_reentrancy_guard_drops_nested_logs_and_resets_afterward() {
        let guard = CallbackReentrancyGuard::enter().expect("first entry should succeed");
        assert!(CallbackReentrancyGuard::enter().is_none());
        drop(guard);
        assert!(CallbackReentrancyGuard::enter().is_some());
    }

    #[test]
    fn log_level_maps_to_and_from_log_facade_levels() {
        for level in [
            LogLevel::Off,
            LogLevel::Error,
            LogLevel::Warn,
            LogLevel::Info,
            LogLevel::Debug,
            LogLevel::Trace,
        ] {
            let filter: log::LevelFilter = level.into();
            assert_eq!(LogLevel::from(filter), level);
        }
    }
}
