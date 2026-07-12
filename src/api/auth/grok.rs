//! Adapter for the official Grok CLI's credential store (`~/.grok/auth.json`).
//!
//! Rather than running its own OAuth login flow, this piggybacks on the login
//! the user already performed with `grok`: it reads the CLI's stored tokens,
//! refreshes them against the same OIDC issuer when they near expiry, and
//! writes the rotated tokens back into the store (surgically — only the token
//! fields of the matching entry change) so this app and the CLI keep sharing
//! one valid session.
//!
//! Store formats accepted (newest first):
//! - Current: a top-level object keyed by `"<issuer>::<client_id>"`, whose
//!   value holds `key` (access token), `refresh_token`, `expires_at`
//!   (ISO-8601), plus `oidc_issuer` / `oidc_client_id`.
//! - Legacy: an entry keyed `"https://accounts.x.ai/sign-in"`, or token fields
//!   at the top level (`access_token`/`token`, optional `refresh`).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use super::oauth;
use crate::ApiError;

/// Fallback OIDC issuer when the store entry does not record one.
const DEFAULT_ISSUER: &str = "https://auth.x.ai";
/// Timeout for the token-refresh request (independent of the LLM request
/// timeouts; a refresh is a small round-trip to the auth server).
const REFRESH_TIMEOUT: Duration = Duration::from_secs(15);

/// The user-facing recovery step for every terminal auth failure.
const SIGN_IN_HINT: &str = "Run `grok` in a terminal and sign in again.";

/// Tokens and OIDC coordinates parsed out of one store entry.
#[derive(Debug, Clone, PartialEq)]
struct TokenState {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<SystemTime>,
}

/// Which entry of the store file the tokens came from, so a refresh can write
/// back to the same place.
#[derive(Debug, Clone, PartialEq)]
enum EntryLocation {
    /// Value object under this top-level key.
    Keyed(String),
    /// Token fields directly at the top level (oldest legacy format).
    TopLevel,
}

#[derive(Debug, Clone, PartialEq)]
struct ParsedStore {
    tokens: TokenState,
    location: EntryLocation,
    token_url: String,
    client_id: String,
}

/// Bearer-token source backed by the Grok CLI's credential store.
pub struct GrokCliAuth {
    path: PathBuf,
    http: reqwest::Client,
    state: Mutex<ParsedStore>,
}

/// `~/.grok/auth.json` under the platform home directory.
fn default_store_path() -> Result<PathBuf, ApiError> {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    let home = home.filter(|h| !h.is_empty()).ok_or_else(|| {
        ApiError::Auth("cannot locate the home directory to find the Grok CLI credentials".into())
    })?;
    Ok(PathBuf::from(home).join(".grok").join("auth.json"))
}

/// First string among the given field names.
fn first_string(obj: &Value, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|n| obj.get(n).and_then(Value::as_str))
        .map(str::to_owned)
}

/// Parse an `expires_at` value: ISO-8601 string, or a unix number
/// (milliseconds when large enough to be one, else seconds).
fn parse_expires_at(v: &Value) -> Option<SystemTime> {
    match v {
        Value::String(s) => oauth::parse_iso8601_utc(s),
        Value::Number(n) => {
            let n = n.as_u64()?;
            // 10^11 seconds is year ~5138; anything larger must be milliseconds.
            let millis = if n >= 100_000_000_000 { n } else { n.checked_mul(1000)? };
            Some(UNIX_EPOCH + Duration::from_millis(millis))
        }
        _ => None,
    }
}

/// Extract tokens from one candidate object, if it holds an access token.
fn parse_entry(obj: &Value) -> Option<TokenState> {
    let access_token = first_string(obj, &["key", "access_token", "token"])?;
    // Absent expiry = a long-lived legacy token (never refresh). An expiry
    // that is PRESENT but unreadable must not degrade into that same "never
    // refresh" — treat it as already expired so the token is refreshed (or
    // re-read from disk) up front instead of silently 401ing forever once it
    // really lapses.
    let expires_at = obj.get("expires_at").map(|v| match parse_expires_at(v) {
        Some(t) => t,
        None => {
            warn!("unreadable expires_at in Grok credential store ({v}); treating the token as already expired");
            UNIX_EPOCH
        }
    });
    Some(TokenState {
        access_token,
        refresh_token: first_string(obj, &["refresh_token", "refresh"]),
        expires_at,
    })
}

/// Locate and parse the credential entry inside the store document.
fn parse_store(doc: &Value) -> Result<ParsedStore, ApiError> {
    let root = doc.as_object().ok_or_else(|| {
        ApiError::Auth("Grok credential store is not a JSON object".into())
    })?;

    // Current format first: prefer a "<issuer>::<client_id>" key (that shape
    // carries the OIDC coordinates); fall back to any keyed object entry with
    // token fields (legacy "https://accounts.x.ai/sign-in"), then to token
    // fields at the top level.
    let keyed = |want_oidc_key: bool| {
        root.iter().find_map(|(k, v)| {
            if want_oidc_key != k.contains("::") {
                return None;
            }
            parse_entry(v).map(|tokens| (k.clone(), v, tokens))
        })
    };

    if let Some((key, entry, tokens)) = keyed(true).or_else(|| keyed(false)) {
        let client_id = first_string(entry, &["oidc_client_id"])
            .or_else(|| key.split_once("::").map(|(_, id)| id.to_owned()))
            .ok_or_else(|| {
                ApiError::Auth("Grok credential entry has no OAuth client id".into())
            })?;
        let issuer = first_string(entry, &["oidc_issuer"])
            .unwrap_or_else(|| DEFAULT_ISSUER.to_owned());
        return Ok(ParsedStore {
            tokens,
            location: EntryLocation::Keyed(key),
            token_url: token_url(&issuer),
            client_id,
        });
    }

    if let Some(tokens) = parse_entry(doc) {
        let client_id = first_string(doc, &["oidc_client_id", "client_id"]).ok_or_else(|| {
            ApiError::Auth("Grok credential store has no OAuth client id".into())
        })?;
        let issuer =
            first_string(doc, &["oidc_issuer"]).unwrap_or_else(|| DEFAULT_ISSUER.to_owned());
        return Ok(ParsedStore {
            tokens,
            location: EntryLocation::TopLevel,
            token_url: token_url(&issuer),
            client_id,
        });
    }

    Err(ApiError::Auth(format!(
        "no Grok credentials found in the store. {SIGN_IN_HINT}"
    )))
}

fn token_url(issuer: &str) -> String {
    format!("{}/oauth2/token", issuer.trim_end_matches('/'))
}

/// Read and parse the store file at `path`.
fn load_store(path: &Path) -> Result<ParsedStore, ApiError> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        ApiError::Auth(format!(
            "cannot read Grok credentials at {}: {e}. {SIGN_IN_HINT}",
            path.display()
        ))
    })?;
    let doc: Value = serde_json::from_str(&raw).map_err(|e| {
        ApiError::Auth(format!(
            "Grok credential store at {} is not valid JSON: {e}",
            path.display()
        ))
    })?;
    parse_store(&doc)
}

/// Update the matching entry of the store document with rotated tokens,
/// touching only the token fields (everything else — emails, ids, unknown
/// future fields — is preserved for the CLI). `expires_at` keeps the type the
/// store already used (ISO string vs unix-ms number).
fn update_store_doc(doc: &mut Value, location: &EntryLocation, tokens: &TokenState) -> bool {
    let entry = match location {
        EntryLocation::Keyed(key) => match doc.get_mut(key) {
            Some(e) => e,
            None => return false,
        },
        EntryLocation::TopLevel => doc,
    };
    let Some(obj) = entry.as_object_mut() else {
        return false;
    };

    // Write the access token into whichever field name(s) the entry already
    // uses; default to the current CLI's `key` when creating from scratch.
    let mut wrote_access = false;
    for name in ["key", "access_token", "token"] {
        if obj.contains_key(name) {
            obj.insert(name.into(), Value::String(tokens.access_token.clone()));
            wrote_access = true;
        }
    }
    if !wrote_access {
        obj.insert("key".into(), Value::String(tokens.access_token.clone()));
    }

    if let Some(refresh) = &tokens.refresh_token {
        let name = if obj.contains_key("refresh") && !obj.contains_key("refresh_token") {
            "refresh"
        } else {
            "refresh_token"
        };
        obj.insert(name.into(), Value::String(refresh.clone()));
    }

    if let Some(expiry) = tokens.expires_at {
        let as_number = matches!(obj.get("expires_at"), Some(Value::Number(_)));
        let value = if as_number {
            let ms = expiry
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            Value::Number(ms.into())
        } else {
            Value::String(crate::format_utc(expiry))
        };
        obj.insert("expires_at".into(), value);
    }
    true
}

/// Persist rotated tokens back into the store file: re-read it (the CLI may
/// have changed unrelated entries meanwhile), patch the matching entry, and
/// replace the file atomically (temp file + rename) with owner-only
/// permissions.
fn persist_tokens(path: &Path, location: &EntryLocation, tokens: &TokenState) -> std::io::Result<()> {
    let raw = std::fs::read_to_string(path)?;
    let mut doc: Value =
        serde_json::from_str(&raw).map_err(|e| std::io::Error::other(e.to_string()))?;
    if !update_store_doc(&mut doc, location, tokens) {
        return Err(std::io::Error::other(
            "credential entry disappeared from the store",
        ));
    }
    let serialized = serde_json::to_string_pretty(&doc)
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    let tmp = path.with_extension("json.clip-llm.tmp");
    std::fs::write(&tmp, serialized)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)
}

impl GrokCliAuth {
    /// Load the credential store, from `path` when configured or from the
    /// default `~/.grok/auth.json`. Fails fast (at client construction) when
    /// the store is missing or holds no credentials, so a never-logged-in
    /// machine reports a clear startup error instead of failing per request.
    pub fn load(path: Option<PathBuf>) -> Result<Self, ApiError> {
        let path = match path {
            Some(p) => p,
            None => default_store_path()?,
        };
        let state = load_store(&path)?;
        info!(
            "grok credentials loaded from {} (refresh_token={}, expires_at={})",
            path.display(),
            if state.tokens.refresh_token.is_some() { "present" } else { "absent" },
            state
                .tokens
                .expires_at
                .map(crate::format_utc)
                .unwrap_or_else(|| "unknown".into()),
        );
        let http = reqwest::Client::builder()
            .timeout(REFRESH_TIMEOUT)
            .build()?;
        Ok(Self {
            path,
            http,
            state: Mutex::new(state),
        })
    }

    /// The store file backing this provider (for logging).
    pub fn store_path(&self) -> &Path {
        &self.path
    }

    /// Current access token, refreshed first when it is within the expiry
    /// skew. Concurrent callers serialize on the internal lock, so one refresh
    /// happens even when several requests race past an expired token.
    pub async fn bearer(&self) -> Result<String, ApiError> {
        let mut state = self.state.lock().await;
        if !oauth::needs_refresh(state.tokens.expires_at) {
            return Ok(state.tokens.access_token.clone());
        }

        // The CLI (or a previous run) may have refreshed already — prefer a
        // newer, still-valid token from disk over spending our refresh grant.
        if let Ok(fresh) = load_store(&self.path)
            && fresh.tokens.access_token != state.tokens.access_token
            && !oauth::needs_refresh(fresh.tokens.expires_at)
        {
            debug!("adopting newer grok token found on disk");
            *state = fresh;
            return Ok(state.tokens.access_token.clone());
        }

        let Some(refresh_token) = state.tokens.refresh_token.clone() else {
            return Err(ApiError::Auth(format!(
                "the stored Grok token expired and there is no refresh token. {SIGN_IN_HINT}"
            )));
        };

        info!("grok access token near expiry; refreshing");
        let refreshed = oauth::refresh(
            &self.http,
            &state.token_url,
            &state.client_id,
            &refresh_token,
        )
        .await
        .map_err(|e| match e {
            // Rejected refresh = the grant itself is dead; tell the user how
            // to recover instead of surfacing a bare HTTP status.
            ApiError::Auth(msg) => {
                ApiError::Auth(format!("Grok sign-in expired ({msg}). {SIGN_IN_HINT}"))
            }
            other => other,
        })?;

        state.tokens = TokenState {
            access_token: refreshed.access_token.clone(),
            // No rotation in the response = the old refresh token stays valid.
            refresh_token: refreshed
                .refresh_token
                .clone()
                .or(Some(refresh_token)),
            expires_at: refreshed.expires_at(),
        };

        // Best-effort write-back keeps the CLI sharing the rotated session; a
        // failure only costs an extra refresh next time (or a CLI re-login if
        // the server rotates refresh tokens), so it must not fail the request.
        if let Err(e) = persist_tokens(&self.path, &state.location, &state.tokens) {
            warn!(
                "could not write refreshed grok tokens back to {}: {e}",
                self.path.display()
            );
        } else {
            debug!("refreshed grok tokens persisted to {}", self.path.display());
        }

        Ok(state.tokens.access_token.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn current_format_store() -> Value {
        serde_json::json!({
            "https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828": {
                "auth_mode": "oidc",
                "email": "user@example.com",
                "expires_at": "2026-07-12T15:19:19.025Z",
                "key": "access-1",
                "oidc_client_id": "b1a00492-073a-47ea-816f-4c329264a828",
                "oidc_issuer": "https://auth.x.ai",
                "refresh_token": "refresh-1",
                "team_id": "team-1"
            }
        })
    }

    #[test]
    fn parse_current_format() {
        let store = parse_store(&current_format_store()).unwrap();
        assert_eq!(store.tokens.access_token, "access-1");
        assert_eq!(store.tokens.refresh_token.as_deref(), Some("refresh-1"));
        assert_eq!(
            store.tokens.expires_at.map(crate::format_utc).as_deref(),
            Some("2026-07-12T15:19:19.025Z")
        );
        assert_eq!(store.client_id, "b1a00492-073a-47ea-816f-4c329264a828");
        assert_eq!(store.token_url, "https://auth.x.ai/oauth2/token");
        assert_eq!(
            store.location,
            EntryLocation::Keyed(
                "https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828".into()
            )
        );
    }

    #[test]
    fn parse_current_format_client_id_from_key_suffix() {
        // Entry without oidc_client_id: the id comes from the map key.
        let doc = serde_json::json!({
            "https://auth.x.ai::the-client-id": { "key": "a" }
        });
        let store = parse_store(&doc).unwrap();
        assert_eq!(store.client_id, "the-client-id");
        assert_eq!(store.token_url, "https://auth.x.ai/oauth2/token");
    }

    #[test]
    fn parse_legacy_keyed_format() {
        let doc = serde_json::json!({
            "https://accounts.x.ai/sign-in": {
                "access_token": "legacy-access",
                "oidc_client_id": "cid"
            }
        });
        let store = parse_store(&doc).unwrap();
        assert_eq!(store.tokens.access_token, "legacy-access");
        assert!(store.tokens.refresh_token.is_none());
        assert!(store.tokens.expires_at.is_none());
        // No oidc_issuer recorded -> default issuer.
        assert_eq!(store.token_url, "https://auth.x.ai/oauth2/token");
    }

    #[test]
    fn parse_top_level_format_with_numeric_expiry() {
        let doc = serde_json::json!({
            "token": "top-access",
            "refresh": "top-refresh",
            "expires_at": 1_752_345_600_000u64,
            "client_id": "cid"
        });
        let store = parse_store(&doc).unwrap();
        assert_eq!(store.tokens.access_token, "top-access");
        assert_eq!(store.tokens.refresh_token.as_deref(), Some("top-refresh"));
        assert_eq!(store.location, EntryLocation::TopLevel);
        assert_eq!(
            store.tokens.expires_at,
            Some(UNIX_EPOCH + Duration::from_millis(1_752_345_600_000))
        );
    }

    #[test]
    fn parse_numeric_expiry_in_seconds() {
        let doc = serde_json::json!({
            "token": "a",
            "expires_at": 1_752_345_600u64,
            "client_id": "cid"
        });
        let store = parse_store(&doc).unwrap();
        assert_eq!(
            store.tokens.expires_at,
            Some(UNIX_EPOCH + Duration::from_secs(1_752_345_600))
        );
    }

    #[test]
    fn parse_unreadable_expiry_means_already_expired() {
        // "Present but unparseable" must trigger an up-front refresh, not
        // degrade into the legacy "no expiry = never refresh" behavior.
        for bad in [
            serde_json::json!("2026-07-12T15:19:19+00:00"), // offset form, not Z
            serde_json::json!(1_752_345_600.5),             // float unix time
            serde_json::json!(true),
        ] {
            let doc = serde_json::json!({
                "token": "a", "client_id": "cid", "expires_at": bad
            });
            let store = parse_store(&doc).unwrap();
            assert_eq!(store.tokens.expires_at, Some(UNIX_EPOCH), "value: {bad}");
        }
        // Genuinely absent stays None (long-lived legacy token).
        let doc = serde_json::json!({"token": "a", "client_id": "cid"});
        assert_eq!(parse_store(&doc).unwrap().tokens.expires_at, None);
    }

    #[test]
    fn parse_rejects_stores_without_credentials() {
        for doc in [
            serde_json::json!({}),
            serde_json::json!({"unrelated": {"foo": 1}}),
            serde_json::json!([1, 2, 3]),
        ] {
            assert!(matches!(parse_store(&doc), Err(ApiError::Auth(_))), "doc: {doc}");
        }
    }

    #[test]
    fn update_store_preserves_other_fields_and_entries() {
        let mut doc = current_format_store();
        // A second, unrelated entry must survive untouched.
        doc.as_object_mut().unwrap().insert(
            "https://other.example::x".into(),
            serde_json::json!({"key": "other-access"}),
        );

        let location = EntryLocation::Keyed(
            "https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828".into(),
        );
        let new_expiry = UNIX_EPOCH + Duration::from_millis(1_800_000_000_000);
        let tokens = TokenState {
            access_token: "access-2".into(),
            refresh_token: Some("refresh-2".into()),
            expires_at: Some(new_expiry),
        };
        assert!(update_store_doc(&mut doc, &location, &tokens));

        let entry = &doc["https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828"];
        assert_eq!(entry["key"], "access-2");
        assert_eq!(entry["refresh_token"], "refresh-2");
        // The store used a string expiry, so the update stays a string.
        assert_eq!(entry["expires_at"], crate::format_utc(new_expiry));
        // Non-token fields are preserved verbatim.
        assert_eq!(entry["email"], "user@example.com");
        assert_eq!(entry["auth_mode"], "oidc");
        assert_eq!(entry["team_id"], "team-1");
        // The unrelated entry is untouched.
        assert_eq!(doc["https://other.example::x"]["key"], "other-access");
    }

    #[test]
    fn update_store_keeps_numeric_expiry_numeric() {
        let mut doc = serde_json::json!({
            "token": "a",
            "expires_at": 1_752_345_600_000u64,
            "client_id": "cid"
        });
        let tokens = TokenState {
            access_token: "b".into(),
            refresh_token: None,
            expires_at: Some(UNIX_EPOCH + Duration::from_millis(1_800_000_000_000)),
        };
        assert!(update_store_doc(&mut doc, &EntryLocation::TopLevel, &tokens));
        assert_eq!(doc["token"], "b");
        assert_eq!(doc["expires_at"], 1_800_000_000_000u64);
    }

    #[test]
    fn update_store_uses_existing_refresh_field_name() {
        let mut doc = serde_json::json!({
            "token": "a",
            "refresh": "old",
            "client_id": "cid"
        });
        let tokens = TokenState {
            access_token: "b".into(),
            refresh_token: Some("new".into()),
            expires_at: None,
        };
        assert!(update_store_doc(&mut doc, &EntryLocation::TopLevel, &tokens));
        assert_eq!(doc["refresh"], "new");
        assert!(doc.get("refresh_token").is_none());
    }

    #[test]
    fn update_store_missing_entry_fails() {
        let mut doc = serde_json::json!({});
        let tokens = TokenState {
            access_token: "a".into(),
            refresh_token: None,
            expires_at: None,
        };
        assert!(!update_store_doc(
            &mut doc,
            &EntryLocation::Keyed("gone".into()),
            &tokens
        ));
    }

    #[test]
    fn persist_tokens_roundtrip_via_file() {
        let dir = std::env::temp_dir().join(format!("clip-llm-grok-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        std::fs::write(&path, current_format_store().to_string()).unwrap();

        let location = EntryLocation::Keyed(
            "https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828".into(),
        );
        let tokens = TokenState {
            access_token: "persisted-access".into(),
            refresh_token: Some("persisted-refresh".into()),
            expires_at: None,
        };
        persist_tokens(&path, &location, &tokens).unwrap();

        let reparsed = load_store(&path).unwrap();
        assert_eq!(reparsed.tokens.access_token, "persisted-access");
        assert_eq!(reparsed.tokens.refresh_token.as_deref(), Some("persisted-refresh"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "store must stay owner-only");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
