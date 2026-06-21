//! Engine error type.

use std::fmt;

#[derive(Debug)]
pub enum EngineError {
    Windows(windows_core::Error),
    NoDevice,
    FormatNotSupported(String),
    PinNotFound,
    Config(String),
    Backend(String),
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::Windows(e) => write!(f, "windows error: {e}"),
            EngineError::NoDevice => write!(f, "no matching audio device found"),
            EngineError::FormatNotSupported(s) => write!(f, "format not supported: {s}"),
            EngineError::PinNotFound => write!(f, "no suitable kernel-streaming pin found"),
            EngineError::Config(s) => write!(f, "invalid configuration: {s}"),
            EngineError::Backend(s) => write!(f, "backend error: {s}"),
        }
    }
}

impl std::error::Error for EngineError {}

impl From<windows_core::Error> for EngineError {
    fn from(e: windows_core::Error) -> Self {
        EngineError::Windows(e)
    }
}

pub type Result<T> = std::result::Result<T, EngineError>;
