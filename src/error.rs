//! One error type. Every failure that a user could plausibly cause carries a
//! sentence explaining what to do about it, because a failure whose reason is
//! missing from the message costs more than the failure itself.

use std::fmt;

/// Why a call failed, where the answer changes what to do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ErrorKind {
    #[default]
    Other,
    /// The call ran past its deadline and was killed. Worth its own kind
    /// because asking again means waiting exactly as long again, which is the
    /// one failure where a retry costs more than it can possibly win.
    TimedOut,
    /// The CLI itself could not answer: a non-zero exit, or an error event in
    /// place of a message. Distinct from an answer that arrived and could not
    /// be parsed, which is what the retry exists for and which a model
    /// corrects readily when told what was wrong. Nothing about a refusal, a
    /// quota, or a crash is corrected by being asked the same thing again.
    CallFailed,
    /// A write failed locally, but the destination could not be reread to tell
    /// whether it landed. Repeating it blindly could duplicate the write.
    UncertainWrite,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SparError {
    message: String,
    kind: ErrorKind,
}

impl SparError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: ErrorKind::Other,
        }
    }

    pub fn timed_out(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: ErrorKind::TimedOut,
        }
    }

    /// The CLI could not answer at all, as opposed to answering unusably.
    pub fn call_failed(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: ErrorKind::CallFailed,
        }
    }

    pub fn uncertain_write(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: ErrorKind::UncertainWrite,
        }
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Whether asking the same thing again could plausibly go better.
    pub fn worth_retrying(&self) -> bool {
        !matches!(self.kind, ErrorKind::TimedOut | ErrorKind::UncertainWrite)
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    /// The last line of a multi-line failure. Useful when a nested command's
    /// own error is the interesting part and the preamble is not.
    pub fn last_line(&self) -> &str {
        self.message.lines().next_back().unwrap_or(&self.message)
    }

    /// The first line, for one-line status output.
    pub fn first_line(&self) -> &str {
        self.message.lines().next().unwrap_or(&self.message)
    }
}

impl fmt::Display for SparError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SparError {}

impl From<std::io::Error> for SparError {
    fn from(e: std::io::Error) -> Self {
        SparError::new(e.to_string())
    }
}

impl From<serde_json::Error> for SparError {
    fn from(e: serde_json::Error) -> Self {
        SparError::new(format!("invalid JSON: {e}"))
    }
}

impl From<toml::de::Error> for SparError {
    fn from(e: toml::de::Error) -> Self {
        SparError::new(format!("invalid TOML: {e}"))
    }
}

pub type Result<T> = std::result::Result<T, SparError>;

/// Build a `SparError` with `format!` syntax.
#[macro_export]
macro_rules! spar_err {
    ($($arg:tt)*) => { $crate::error::SparError::new(format!($($arg)*)) };
}

/// Return early with a `SparError`.
#[macro_export]
macro_rules! bail {
    ($($arg:tt)*) => { return Err($crate::spar_err!($($arg)*)) };
}
