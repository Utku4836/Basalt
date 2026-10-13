use std::path::Path;

use serde::Serialize;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    InvalidInput,
    NotFound,
    Conflict,
    Busy,
    CorruptData,
    Filesystem,
    Cancelled,
    Process,
    Network,
    Integrity,
    Recovery,
    Unsupported,
}

#[derive(Debug, Clone, thiserror::Error, Serialize)]
#[error("{message}")]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
    pub path: Option<String>,
    pub os_code: Option<i32>,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            path: None,
            os_code: None,
        }
    }

    pub fn io(action: &str, path: &Path, error: std::io::Error) -> Self {
        Self {
            kind: ErrorKind::Filesystem,
            message: format!("{action}: {error}"),
            path: Some(path.display().to_string()),
            os_code: error.raw_os_error(),
        }
    }

    pub fn corrupt(error: impl std::fmt::Display) -> Self {
        Self::new(ErrorKind::CorruptData, error.to_string())
    }
}
