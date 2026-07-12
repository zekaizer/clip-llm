//! Bearer-token sources for the LLM client.
//!
//! [`TokenProvider`] abstracts where the `Authorization: Bearer` value comes
//! from, so the HTTP client never knows the difference between a static API
//! key and an OAuth token that must be refreshed. Adding a provider means
//! adding a variant here plus an adapter module (see [`grok`]) — the generic
//! OAuth machinery in [`oauth`] is provider-independent and reusable.

pub mod grok;
pub mod oauth;

use crate::ApiError;

/// Where the bearer token for API requests comes from.
pub enum TokenProvider {
    /// A static API key from config/env (`None` = send no Authorization header).
    Static(Option<String>),
    /// OAuth tokens piggybacked on the official Grok CLI's credential store,
    /// refreshed automatically when near expiry.
    GrokCli(grok::GrokCliAuth),
}

impl TokenProvider {
    /// Resolve the current bearer token, refreshing it first when the source
    /// requires it. `Ok(None)` means "send no Authorization header".
    pub async fn bearer(&self) -> Result<Option<String>, ApiError> {
        match self {
            TokenProvider::Static(key) => Ok(key.clone()),
            TokenProvider::GrokCli(auth) => auth.bearer().await.map(Some),
        }
    }

    /// Short human-readable description for startup logging (never a secret).
    pub fn describe(&self) -> String {
        match self {
            TokenProvider::Static(Some(_)) => "static api_key".to_string(),
            TokenProvider::Static(None) => "none".to_string(),
            TokenProvider::GrokCli(auth) => format!("grok-cli oauth ({})", auth.store_path().display()),
        }
    }
}
