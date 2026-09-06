//! File-list clipboard ingestion: turns files copied in Finder/Explorer into
//! [`ClipboardContent`] the pipeline already understands (text and PNG images).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracing::{info, warn};

use crate::images::encode::{check_pixel_budget, encode_rgba_for_upload, MAX_IMAGE_PIXELS};
use crate::{ClipboardContent, ClipboardError};

/// Per-file cap for text files; larger files are refused rather than
/// truncated, since a silently cut input yields a confidently wrong answer.
pub const MAX_TEXT_FILE_BYTES: u64 = 1024 * 1024;
/// Cap on the combined text of all files in one clipboard.
pub const MAX_TOTAL_TEXT_BYTES: u64 = 2 * 1024 * 1024;
/// Cap for a PNG file before decoding.
pub const MAX_IMAGE_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// Build clipboard content from a file list. Text files become the text
/// (one file raw; several joined under `=== name ===` headers), PNG files
/// become images; any unsupported file fails the whole ingest with
/// [`ClipboardError::UnsupportedFiles`] so nothing is dropped silently.
pub fn ingest_files(paths: &[PathBuf]) -> Result<ClipboardContent, ClipboardError> {
    if paths.is_empty() {
        return Err(ClipboardError::NoTextInClipboard);
    }
    let mut texts: Vec<(String, String)> = Vec::new();
    let mut images: Vec<Arc<Vec<u8>>> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let mut unsupported: Vec<String> = Vec::new();
    let mut total_text: u64 = 0;

    for path in paths {
        let name = display_name(path);
        let meta = std::fs::metadata(path).map_err(|e| ClipboardError::FileReadFailed {
            name: name.clone(),
            reason: e.to_string(),
        })?;
        if meta.is_dir() {
            unsupported.push(name);
            continue;
        }
        if is_png_name(path) {
            if meta.len() > MAX_IMAGE_FILE_BYTES {
                return Err(ClipboardError::FileTooLarge {
                    name,
                    limit_bytes: MAX_IMAGE_FILE_BYTES,
                });
            }
            let bytes = read(path, &name)?;
            match decode_png_rgba(&bytes)? {
                Some((rgba, w, h)) => {
                    images.push(Arc::new(encode_rgba_for_upload(rgba, w, h)?));
                    names.push(name);
                }
                None => {
                    warn!("file {name}: .png extension but not a decodable PNG");
                    unsupported.push(name);
                }
            }
            continue;
        }
        if meta.len() > MAX_TEXT_FILE_BYTES {
            return Err(ClipboardError::FileTooLarge {
                name,
                limit_bytes: MAX_TEXT_FILE_BYTES,
            });
        }
        let bytes = read(path, &name)?;
        let Some(text) = as_text(&bytes) else {
            unsupported.push(name);
            continue;
        };
        total_text += text.len() as u64;
        if total_text > MAX_TOTAL_TEXT_BYTES {
            return Err(ClipboardError::FileTooLarge {
                name,
                limit_bytes: MAX_TOTAL_TEXT_BYTES,
            });
        }
        names.push(name.clone());
        if !text.trim().is_empty() {
            texts.push((name, text.to_string()));
        }
    }

    if !unsupported.is_empty() {
        return Err(ClipboardError::UnsupportedFiles(unsupported));
    }
    let text = match texts.len() {
        0 => None,
        1 => Some(texts.remove(0).1),
        _ => Some(
            texts
                .iter()
                .map(|(name, body)| format!("=== {name} ===\n{body}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
    };
    if text.is_none() && images.is_empty() {
        return Err(ClipboardError::EmptyCopy);
    }
    info!(
        "ingested {} file(s): {} text chars, {} image(s)",
        names.len(),
        text.as_ref().map_or(0, String::len),
        images.len()
    );
    Ok(ClipboardContent {
        text,
        images,
        files: names,
    })
}

fn read(path: &Path, name: &str) -> Result<Vec<u8>, ClipboardError> {
    std::fs::read(path).map_err(|e| ClipboardError::FileReadFailed {
        name: name.to_string(),
        reason: e.to_string(),
    })
}

/// File name for messages and the source badge.
fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn is_png_name(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.to_string_lossy().eq_ignore_ascii_case("png"))
}

/// Text means valid UTF-8 with no NUL byte; anything else is treated as
/// binary regardless of extension.
fn as_text(bytes: &[u8]) -> Option<&str> {
    if bytes.contains(&0) {
        return None;
    }
    std::str::from_utf8(bytes).ok()
}

/// Decode a PNG into 8-bit RGBA. `Ok(None)` for anything that is not a
/// decodable PNG; `Err` only for an image over the pixel budget.
fn decode_png_rgba(bytes: &[u8]) -> Result<Option<(Vec<u8>, u32, u32)>, ClipboardError> {
    // The decoder may allocate at most one RGBA buffer at the pixel budget;
    // the header check below is what actually refuses larger images.
    let limits = png::Limits {
        bytes: (MAX_IMAGE_PIXELS * 4) as usize,
    };
    let mut decoder = png::Decoder::new_with_limits(std::io::Cursor::new(bytes), limits);
    // Expand palettes / low bit depths and strip 16-bit so every color type
    // below arrives as 8-bit samples.
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let Ok(mut reader) = decoder.read_info() else {
        return Ok(None);
    };
    check_pixel_budget(reader.info().width, reader.info().height)?;
    let Some(size) = reader.output_buffer_size() else {
        return Ok(None);
    };
    let mut buf = vec![0u8; size];
    let Ok(info) = reader.next_frame(&mut buf) else {
        return Ok(None);
    };
    let px = &buf[..info.buffer_size()];
    let rgba = match info.color_type {
        png::ColorType::Rgba => px.to_vec(),
        png::ColorType::Rgb => px
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|c| [c[0], c[1], c[2], 255])
            .collect(),
        png::ColorType::Grayscale => px.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::GrayscaleAlpha => px
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|c| [c[0], c[0], c[0], c[1]])
            .collect(),
        // normalize_to_color8 expands palettes, so this cannot occur.
        png::ColorType::Indexed => return Ok(None),
    };
    Ok(Some((rgba, info.width, info.height)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn tmp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "clip-llm-files-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, bytes).unwrap();
        p
    }

    fn tiny_png() -> Vec<u8> {
        // 2x1 RGBA: red, green.
        crate::images::encode::rgba_to_png(&[255, 0, 0, 255, 0, 255, 0, 255], 2, 1).unwrap()
    }

    #[test]
    fn single_text_file_is_raw_content() {
        let d = tmp_dir();
        let p = write(&d, "notes.md", "# Title\n\nbody\n".as_bytes());
        let c = ingest_files(&[p]).unwrap();
        assert_eq!(c.text.as_deref(), Some("# Title\n\nbody\n"));
        assert!(c.images.is_empty());
        assert_eq!(c.files, vec!["notes.md".to_string()]);
    }

    #[test]
    fn multiple_text_files_get_headers_in_order() {
        let d = tmp_dir();
        let a = write(&d, "a.rs", b"fn a() {}\n");
        let b = write(&d, "b.toml", b"k = 1\n");
        let c = ingest_files(&[a, b]).unwrap();
        assert_eq!(
            c.text.as_deref(),
            Some("=== a.rs ===\nfn a() {}\n\n=== b.toml ===\nk = 1\n"),
        );
        assert_eq!(c.files, vec!["a.rs".to_string(), "b.toml".to_string()]);
    }

    #[test]
    fn extensionless_utf8_file_counts_as_text() {
        let d = tmp_dir();
        let p = write(&d, "Makefile", "all:\n\techo hi\n".as_bytes());
        let c = ingest_files(&[p]).unwrap();
        assert_eq!(c.text.as_deref(), Some("all:\n\techo hi\n"));
    }

    #[test]
    fn png_file_becomes_image() {
        let d = tmp_dir();
        let p = write(&d, "shot.png", &tiny_png());
        let c = ingest_files(&[p]).unwrap();
        assert!(c.text.is_none());
        assert_eq!(c.images.len(), 1);
        assert!(c.is_image_only());
        // Re-encoded through the same PNG path the clipboard image uses.
        assert!(c.images[0].starts_with(&[0x89, b'P', b'N', b'G']));
        assert_eq!(c.files, vec!["shot.png".to_string()]);
    }

    /// A syntactically valid PNG whose header claims `w x h` pixels but whose
    /// IDAT holds no rows: enough for the header to parse, nothing to decode.
    fn png_with_header(w: u32, h: u32) -> Vec<u8> {
        fn crc32(data: &[u8]) -> u32 {
            let mut crc = 0xFFFF_FFFFu32;
            for &b in data {
                crc ^= u32::from(b);
                for _ in 0..8 {
                    crc = if crc & 1 == 1 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
                }
            }
            !crc
        }
        fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
            out.extend_from_slice(&(body.len() as u32).to_be_bytes());
            let mut typed = kind.to_vec();
            typed.extend_from_slice(body);
            out.extend_from_slice(&typed);
            out.extend_from_slice(&crc32(&typed).to_be_bytes());
        }
        let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA, no interlace
        chunk(&mut out, b"IHDR", &ihdr);
        chunk(&mut out, b"IDAT", &[0x78, 0x9C, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01]); // empty zlib stream
        chunk(&mut out, b"IEND", &[]);
        out
    }

    #[test]
    fn oversized_png_is_refused_from_the_header() {
        let d = tmp_dir();
        let p = write(&d, "huge.png", &png_with_header(20_000, 20_000));
        match ingest_files(&[p]) {
            Err(ClipboardError::ImageTooLarge { width: 20_000, height: 20_000, .. }) => {}
            other => panic!("expected ImageTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn png_decode_handles_rgb_and_gray() {
        // RGB 1x1 and grayscale 1x1 encoded by hand via the png crate.
        fn enc(color: png::ColorType, data: &[u8]) -> Vec<u8> {
            let mut out = Vec::new();
            let mut e = png::Encoder::new(&mut out, 1, 1);
            e.set_color(color);
            e.set_depth(png::BitDepth::Eight);
            let mut w = e.write_header().unwrap();
            w.write_image_data(data).unwrap();
            drop(w);
            out
        }
        let (rgba, w, h) = decode_png_rgba(&enc(png::ColorType::Rgb, &[10, 20, 30])).unwrap().unwrap();
        assert_eq!((w, h), (1, 1));
        assert_eq!(rgba, vec![10, 20, 30, 255]);
        let (rgba, ..) = decode_png_rgba(&enc(png::ColorType::Grayscale, &[77])).unwrap().unwrap();
        assert_eq!(rgba, vec![77, 77, 77, 255]);
        let (rgba, ..) =
            decode_png_rgba(&enc(png::ColorType::GrayscaleAlpha, &[77, 128])).unwrap().unwrap();
        assert_eq!(rgba, vec![77, 77, 77, 128]);
    }

    #[test]
    fn text_and_png_together() {
        let d = tmp_dir();
        let t = write(&d, "caption.txt", b"see picture");
        let i = write(&d, "pic.PNG", &tiny_png());
        let c = ingest_files(&[t, i]).unwrap();
        assert_eq!(c.text.as_deref(), Some("see picture"));
        assert_eq!(c.images.len(), 1);
        assert_eq!(c.files.len(), 2);
    }

    #[test]
    fn binary_file_is_unsupported_and_named() {
        let d = tmp_dir();
        let ok = write(&d, "ok.txt", b"fine");
        let bin = write(&d, "blob.bin", &[0u8, 1, 2, 3, 0xff]);
        let pdf = write(&d, "doc.pdf", b"%PDF-1.4\n\x00\x00");
        match ingest_files(&[ok, bin, pdf]) {
            Err(ClipboardError::UnsupportedFiles(names)) => {
                assert_eq!(names, vec!["blob.bin".to_string(), "doc.pdf".to_string()]);
            }
            other => panic!("expected UnsupportedFiles, got {other:?}"),
        }
    }

    #[test]
    fn png_extension_with_bad_bytes_is_unsupported() {
        let d = tmp_dir();
        let p = write(&d, "fake.png", b"not a png at all");
        assert!(matches!(
            ingest_files(&[p]),
            Err(ClipboardError::UnsupportedFiles(n)) if n == vec!["fake.png".to_string()]
        ));
    }

    #[test]
    fn directory_is_unsupported() {
        let d = tmp_dir();
        let sub = d.join("folder");
        fs::create_dir_all(&sub).unwrap();
        assert!(matches!(
            ingest_files(&[sub]),
            Err(ClipboardError::UnsupportedFiles(n)) if n == vec!["folder".to_string()]
        ));
    }

    #[test]
    fn non_utf8_text_is_unsupported() {
        let d = tmp_dir();
        // EUC-KR bytes for a Korean syllable: valid text elsewhere, not UTF-8.
        let p = write(&d, "legacy.txt", &[0xb0, 0xa1, 0x0a]);
        assert!(matches!(ingest_files(&[p]), Err(ClipboardError::UnsupportedFiles(_))));
    }

    #[test]
    fn oversized_text_file_is_refused() {
        let d = tmp_dir();
        let big = vec![b'x'; MAX_TEXT_FILE_BYTES as usize + 1];
        let p = write(&d, "big.log", &big);
        match ingest_files(&[p]) {
            Err(ClipboardError::FileTooLarge { name, limit_bytes }) => {
                assert_eq!(name, "big.log");
                assert_eq!(limit_bytes, MAX_TEXT_FILE_BYTES);
            }
            other => panic!("expected FileTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn combined_text_over_total_cap_is_refused() {
        let d = tmp_dir();
        let half = vec![b'y'; (MAX_TOTAL_TEXT_BYTES / 2) as usize];
        let a = write(&d, "a.txt", &half);
        let b = write(&d, "b.txt", &half);
        let c = write(&d, "c.txt", b"one more byte");
        match ingest_files(&[a, b, c]) {
            Err(ClipboardError::FileTooLarge { name, limit_bytes }) => {
                assert_eq!(name, "c.txt");
                assert_eq!(limit_bytes, MAX_TOTAL_TEXT_BYTES);
            }
            other => panic!("expected FileTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn missing_file_is_read_failure() {
        let d = tmp_dir();
        let p = d.join("gone.txt");
        assert!(matches!(
            ingest_files(&[p]),
            Err(ClipboardError::FileReadFailed { name, .. }) if name == "gone.txt"
        ));
    }

    #[test]
    fn empty_list_is_empty_clipboard() {
        assert!(matches!(ingest_files(&[]), Err(ClipboardError::NoTextInClipboard)));
    }

    #[test]
    fn whitespace_only_text_file_is_unsupported() {
        let d = tmp_dir();
        let p = write(&d, "blank.txt", b"  \n\t\n");
        assert!(matches!(ingest_files(&[p]), Err(ClipboardError::EmptyCopy)));
    }
}
