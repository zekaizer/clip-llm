//! Provider-independent OAuth2 helpers: the `refresh_token` grant and the
//! ISO-8601 timestamp conversions credential stores tend to need.
//!
//! Kept free of any provider-specific knowledge (file formats, issuers,
//! client ids) so future CLI-piggyback providers can reuse it as-is.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use tracing::debug;

use crate::ApiError;

/// Refresh this long before the recorded expiry, absorbing clock skew and the
/// request's own latency.
pub const REFRESH_SKEW: Duration = Duration::from_secs(120);

/// A successful token-endpoint response for the `refresh_token` grant.
/// `refresh_token` is optional: when the server does not rotate it, the caller
/// keeps using the old one.
#[derive(Debug, Deserialize)]
pub struct RefreshedTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Lifetime of `access_token` in seconds from now.
    pub expires_in: Option<u64>,
}

impl RefreshedTokens {
    /// Absolute expiry computed from `expires_in`, when the server sent one.
    pub fn expires_at(&self) -> Option<SystemTime> {
        self.expires_in.map(|s| SystemTime::now() + Duration::from_secs(s))
    }
}

/// Whether a token with this recorded expiry needs a refresh now (within
/// [`REFRESH_SKEW`] of expiry). `None` = no recorded expiry = never refresh.
pub fn needs_refresh(expires_at: Option<SystemTime>) -> bool {
    match expires_at {
        None => false,
        Some(t) => SystemTime::now() + REFRESH_SKEW >= t,
    }
}

/// Execute the OAuth2 `refresh_token` grant against `token_url` for a public
/// client (no client secret — PKCE-style CLI clients refresh with the id
/// alone). Returns [`ApiError::Auth`] with a user-actionable message when the
/// server rejects the refresh token (expired/revoked grant).
pub async fn refresh(
    http: &reqwest::Client,
    token_url: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<RefreshedTokens, ApiError> {
    debug!("refreshing OAuth token via {token_url}");
    let resp = http
        .post(token_url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ])
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        debug!("token refresh rejected: HTTP {} body: {body}", status.as_u16());
        return Err(ApiError::Auth(format!(
            "OAuth token refresh was rejected (HTTP {})",
            status.as_u16()
        )));
    }

    let body = resp.text().await?;
    serde_json::from_str::<RefreshedTokens>(&body)
        .map_err(|e| ApiError::Auth(format!("unreadable token refresh response: {e}")))
}

/// Parse an ISO-8601 UTC timestamp (`YYYY-MM-DDThh:mm:ss[.fff…]Z`) into a
/// [`SystemTime`]. The inverse of [`crate::format_utc`]; fractional seconds
/// beyond milliseconds are truncated. Returns `None` on any deviation from
/// that shape (this parses credential-store timestamps, not arbitrary dates).
pub fn parse_iso8601_utc(s: &str) -> Option<SystemTime> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;

    let mut date_it = date.split('-');
    let year: i64 = date_it.next()?.parse().ok()?;
    let month: u32 = date_it.next()?.parse().ok()?;
    let day: u32 = date_it.next()?.parse().ok()?;
    if date_it.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let (hms, frac) = match time.split_once('.') {
        Some((hms, frac)) => (hms, Some(frac)),
        None => (time, None),
    };
    let mut time_it = hms.split(':');
    let hour: u64 = time_it.next()?.parse().ok()?;
    let min: u64 = time_it.next()?.parse().ok()?;
    let sec: u64 = time_it.next()?.parse().ok()?;
    if time_it.next().is_some() || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    let millis: u64 = match frac {
        None => 0,
        Some(f) => {
            if f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let ms: String = format!("{f}000").chars().take(3).collect();
            ms.parse().ok()?
        }
    };

    // days_from_civil (Howard Hinnant): (year, month, day) -> days since epoch.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    let secs = days.checked_mul(86_400)? + (hour * 3600 + min * 60 + sec) as i64;
    if secs < 0 {
        return None; // pre-epoch timestamps cannot occur in a live credential store
    }
    Some(UNIX_EPOCH + Duration::from_millis(secs as u64 * 1000 + millis))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_iso8601_roundtrips_format_utc() {
        for t in [
            UNIX_EPOCH,
            UNIX_EPOCH + Duration::from_millis(1_700_000_000_123),
            UNIX_EPOCH + Duration::from_millis(86_399_999),
        ] {
            let s = crate::format_utc(t);
            assert_eq!(parse_iso8601_utc(&s), Some(t), "roundtrip failed for {s}");
        }
    }

    #[test]
    fn parse_iso8601_real_grok_timestamp() {
        // The exact shape the Grok CLI writes.
        let t = parse_iso8601_utc("2026-07-12T15:19:19.025Z").expect("must parse");
        assert_eq!(crate::format_utc(t), "2026-07-12T15:19:19.025Z");
    }

    #[test]
    fn parse_iso8601_without_fraction() {
        let t = parse_iso8601_utc("1970-01-02T00:00:00Z").expect("must parse");
        assert_eq!(t, UNIX_EPOCH + Duration::from_secs(86_400));
    }

    #[test]
    fn parse_iso8601_long_fraction_truncated_to_millis() {
        // Microsecond precision (as in the CLI's create_time) truncates.
        let t = parse_iso8601_utc("2026-06-13T10:58:45.783067Z").expect("must parse");
        assert_eq!(crate::format_utc(t), "2026-06-13T10:58:45.783Z");
    }

    #[test]
    fn parse_iso8601_rejects_garbage() {
        for s in [
            "",
            "not-a-date",
            "2026-07-12 15:19:19Z",  // missing 'T'
            "2026-07-12T15:19:19",   // missing 'Z'
            "2026-13-01T00:00:00Z",  // month out of range
            "2026-07-12T24:00:00Z",  // hour out of range
            "2026-07-12T15:19:19.Z", // empty fraction
            "1969-12-31T23:59:59Z",  // pre-epoch
        ] {
            assert_eq!(parse_iso8601_utc(s), None, "should reject {s:?}");
        }
    }

    #[test]
    fn needs_refresh_respects_skew() {
        // Far future: no refresh.
        assert!(!needs_refresh(Some(SystemTime::now() + Duration::from_secs(3600))));
        // Inside the skew window: refresh.
        assert!(needs_refresh(Some(SystemTime::now() + Duration::from_secs(30))));
        // Already expired: refresh.
        assert!(needs_refresh(Some(SystemTime::now() - Duration::from_secs(30))));
        // No recorded expiry: treat as long-lived.
        assert!(!needs_refresh(None));
    }

    #[test]
    fn refreshed_tokens_parse_minimal_and_full() {
        let full: RefreshedTokens = serde_json::from_str(
            r#"{"access_token":"a","refresh_token":"r","expires_in":3600,"id_token":"x","token_type":"Bearer"}"#,
        )
        .unwrap();
        assert_eq!(full.access_token, "a");
        assert_eq!(full.refresh_token.as_deref(), Some("r"));
        assert_eq!(full.expires_in, Some(3600));
        assert!(full.expires_at().is_some());

        // Servers that neither rotate the refresh token nor report expiry.
        let minimal: RefreshedTokens =
            serde_json::from_str(r#"{"access_token":"a"}"#).unwrap();
        assert!(minimal.refresh_token.is_none());
        assert!(minimal.expires_at().is_none());
    }
}
