//! Bytes behind an `<img src>`, by scheme: inline `data:` URIs, local
//! `file:` URLs (Office writes clipboard images to a temp folder), and
//! `http(s)` on the intranet. Every path is bounded in size and time, since
//! this runs on the clipboard-read thread during a capture.

use std::path::PathBuf;
use std::time::Duration;

/// Bytes accepted from one source; larger files and responses are skipped.
pub const MAX_SOURCE_BYTES: u64 = 8 * 1024 * 1024;
/// Total wait for one remote image (connect + headers + body).
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(2);

/// Decode a `data:` URI's base64 payload. `None` for anything else, for a
/// non-base64 encoding, or for a payload over [`MAX_SOURCE_BYTES`].
pub fn data_uri_bytes(src: &str) -> Option<Vec<u8>> {
    let s = src.trim();
    if !has_scheme(s, "data:") {
        return None;
    }
    let (meta, payload) = s[5..].split_once(',')?;
    if !meta.split(';').any(|p| p.trim().eq_ignore_ascii_case("base64")) {
        return None;
    }
    if payload.len() as u64 > MAX_SOURCE_BYTES / 3 * 4 + 4 {
        return None;
    }
    let compact: Vec<u8> = payload.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(compact).ok()
}

/// Case-insensitive scheme prefix test that never slices inside a character.
fn has_scheme(s: &str, scheme: &str) -> bool {
    s.get(..scheme.len()).is_some_and(|p| p.eq_ignore_ascii_case(scheme))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = if bytes[i] == b'%' && i + 2 < bytes.len() {
            std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        } else {
            None
        };
        match hex {
            Some(b) => {
                out.push(b);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The filesystem path of a `file:` URL, percent-decoded, with the leading
/// slash dropped before a Windows drive (`file:///C:/x` → `C:/x`).
pub fn file_url_path(src: &str) -> Option<PathBuf> {
    let s = src.trim();
    if !has_scheme(s, "file:") {
        return None;
    }
    let rest = &s[5..];
    let path = match rest.strip_prefix("//") {
        Some(after) => {
            let (host, path) = after.find('/').map_or((after, ""), |i| (&after[..i], &after[i..]));
            // A remote share is not a local file.
            if !(host.is_empty() || host.eq_ignore_ascii_case("localhost")) {
                return None;
            }
            path
        }
        None => rest,
    };
    if path.is_empty() {
        return None;
    }
    let decoded = percent_decode(path);
    let b = decoded.as_bytes();
    let drive = b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':';
    Some(PathBuf::from(if drive { &decoded[1..] } else { &decoded[..] }))
}

/// Fetch by scheme. `None` for an unsupported scheme, an unreadable file,
/// a failed or oversized response, or a timeout.
pub fn fetch_source(src: &str) -> Option<Vec<u8>> {
    let s = src.trim();
    if has_scheme(s, "data:") {
        data_uri_bytes(s)
    } else if has_scheme(s, "file:") {
        read_local(&file_url_path(s)?)
    } else if has_scheme(s, "http://") || has_scheme(s, "https://") {
        fetch_http(s)
    } else {
        None
    }
}

fn read_local(path: &std::path::Path) -> Option<Vec<u8>> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_SOURCE_BYTES {
        return None;
    }
    std::fs::read(path).ok()
}

/// Bounded GET: [`FETCH_TIMEOUT`] overall, at most [`MAX_SOURCE_BYTES`]
/// read even when the server sends no `Content-Length`.
fn fetch_http(url: &str) -> Option<Vec<u8>> {
    use std::io::Read;
    let client = reqwest::blocking::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .build()
        .ok()?;
    let resp = client.get(url).send().ok()?;
    if !resp.status().is_success() || resp.content_length().is_some_and(|n| n > MAX_SOURCE_BYTES) {
        return None;
    }
    let mut buf = Vec::new();
    resp.take(MAX_SOURCE_BYTES + 1).read_to_end(&mut buf).ok()?;
    (buf.len() as u64 <= MAX_SOURCE_BYTES).then_some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_uri_base64_payload_is_decoded() {
        assert_eq!(data_uri_bytes("data:image/png;base64,iVBORw0KGgo="), Some(vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]));
        // Whitespace inside the payload (line-wrapped markup) is tolerated.
        assert_eq!(data_uri_bytes("data:image/png;base64,iVBO\nRw0K Ggo="), Some(vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]));
        assert_eq!(data_uri_bytes("DATA:image/jpeg;base64,/9g="), Some(vec![0xFF, 0xD8]));
    }

    #[test]
    fn data_uri_rejects_non_base64_and_other_schemes() {
        assert_eq!(data_uri_bytes("data:image/svg+xml,<svg/>"), None);
        assert_eq!(data_uri_bytes("data:image/png;base64,!!!"), None);
        assert_eq!(data_uri_bytes("https://h/a.png"), None);
        assert_eq!(data_uri_bytes("data:"), None);
    }

    #[test]
    fn file_url_becomes_a_path_on_either_platform() {
        assert_eq!(file_url_path("file:///Users/u/a%20b.png"), Some(PathBuf::from("/Users/u/a b.png")));
        assert_eq!(
            file_url_path("file:///C:/Users/U/AppData/Local/Temp/msohtmlclip1/01/clip_image002.png"),
            Some(PathBuf::from("C:/Users/U/AppData/Local/Temp/msohtmlclip1/01/clip_image002.png"))
        );
        assert_eq!(file_url_path("FILE://localhost/tmp/x.png"), Some(PathBuf::from("/tmp/x.png")));
        assert_eq!(file_url_path("https://h/a.png"), None);
        assert_eq!(file_url_path("file:"), None);
    }

    #[test]
    fn fetch_dispatches_data_and_file_and_refuses_the_rest() {
        assert_eq!(fetch_source("data:image/jpeg;base64,/9g="), Some(vec![0xFF, 0xD8]));
        let dir = std::env::temp_dir().join(format!("clip-llm-fetch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("img one.bin");
        std::fs::write(&path, [1u8, 2, 3]).unwrap();
        let url = format!("file://{}", path.display().to_string().replace(' ', "%20"));
        assert_eq!(fetch_source(&url), Some(vec![1, 2, 3]));
        assert_eq!(fetch_source("file:///definitely/missing/x.png"), None);
        assert_eq!(fetch_source("ftp://h/a.png"), None);
        assert_eq!(fetch_source("about:blank"), None);
        assert_eq!(fetch_source(""), None);
    }
}
