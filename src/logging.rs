//! Progress goes to stderr so stdout stays parseable. `--quiet` silences
//! progress but never silences a warning or an error: those are exactly what
//! quiet mode would otherwise bury.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

static QUIET: AtomicBool = AtomicBool::new(false);
static NO_COLOR: AtomicBool = AtomicBool::new(false);

/// Serialises writes so two triage threads cannot interleave mid-line.
static STDERR_LOCK: Mutex<()> = Mutex::new(());

pub fn set_quiet(value: bool) {
    QUIET.store(value, Ordering::Relaxed);
}

pub fn quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

pub fn init_color() {
    let disabled = std::env::var_os("NO_COLOR").is_some()
        || std::env::var("TERM").map(|t| t == "dumb").unwrap_or(false);
    NO_COLOR.store(disabled, Ordering::Relaxed);
}

fn colored() -> bool {
    !NO_COLOR.load(Ordering::Relaxed)
}

fn emit(line: String) {
    let _guard = STDERR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "{line}");
    let _ = err.flush();
}

/// Progress. Suppressed by `--quiet`.
pub fn log(msg: impl AsRef<str>) {
    if quiet() {
        return;
    }
    emit(format!("spar: {}", msg.as_ref()));
}

/// Secondary progress: true but not interesting. Suppressed by `--quiet`.
pub fn dim(msg: impl AsRef<str>) {
    if quiet() {
        return;
    }
    if colored() {
        emit(format!("\x1b[2mspar: {}\x1b[0m", msg.as_ref()));
    } else {
        emit(format!("spar: {}", msg.as_ref()));
    }
}

/// Serious but not fatal. Never suppressed.
pub fn warn(msg: impl AsRef<str>) {
    if colored() {
        emit(format!("\x1b[33mspar: WARNING:\x1b[0m {}", msg.as_ref()));
    } else {
        emit(format!("spar: WARNING: {}", msg.as_ref()));
    }
}

/// Fatal. Never suppressed.
pub fn error(msg: impl AsRef<str>) {
    if colored() {
        emit(format!("\x1b[31mspar:\x1b[0m {}", msg.as_ref()));
    } else {
        emit(format!("spar: {}", msg.as_ref()));
    }
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::logging::log(format!($($arg)*)) };
}

#[macro_export]
macro_rules! logdim {
    ($($arg:tt)*) => { $crate::logging::dim(format!($($arg)*)) };
}

#[macro_export]
macro_rules! logwarn {
    ($($arg:tt)*) => { $crate::logging::warn(format!($($arg)*)) };
}
