#![deny(unused_must_use)]

pub mod api;
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
