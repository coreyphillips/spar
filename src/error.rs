//! One error type. Every failure that a user could plausibly cause carries a
//! sentence explaining what to do about it, because a failure whose reason is
//! missing from the message costs more than the failure itself.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SparError {
    message: String,
}

impl SparError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
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
