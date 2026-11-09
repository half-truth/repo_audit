//! User-facing errors: message + hint only, never a traceback.

use std::fmt;

#[derive(Debug, Clone)]
pub struct AuditError {
    pub message: String,
    pub hint: String,
}

impl AuditError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            hint: String::new(),
        }
    }

    pub fn with_hint(message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            hint: hint.into(),
        }
    }
}

impl fmt::Display for AuditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for AuditError {}

impl From<std::io::Error> for AuditError {
    fn from(e: std::io::Error) -> Self {
        AuditError::new(e.to_string())
    }
}

impl From<rusqlite::Error> for AuditError {
    fn from(e: rusqlite::Error) -> Self {
        AuditError::new(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, AuditError>;
