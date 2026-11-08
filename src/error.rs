//! Kimo error types.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum KimoError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Usage(String),

    #[error("browser error: {0}")]
    Browser(String),

    #[error("Chrome DevTools Protocol error: {0}")]
    Cdp(String),

    #[error("encoding error: {0}")]
    Encode(String),

    #[error("output verification failed: {0}")]
    Verify(String),
}

pub type Result<T> = std::result::Result<T, KimoError>;
