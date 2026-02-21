#![deny(unused_must_use)]

pub mod api;
pub mod clipboard;
pub mod platform;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PlatformError {
    #[error("accessibility permission required")]
    AccessibilityDenied,

    #[error("copy simulation failed: {0}")]
    CopyFailed(String),
}

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("http request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("unexpected response structure: {0}")]
    ParseError(String),

    #[error("empty response from model")]
    EmptyResponse,
}

#[derive(Debug, Error)]
pub enum ClipboardError {
    #[error("clipboard access failed: {0}")]
    AccessFailed(String),

    #[error("no text in clipboard after copy simulation")]
    NoTextAfterCopy,

    #[error("clipboard write failed: {0}")]
    WriteFailed(String),

    #[error("copy simulation failed: {0}")]
    CopyFailed(#[from] PlatformError),
}
