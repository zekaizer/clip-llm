//! Meaningfulness filter for HTML-embedded images. Applies only to images
//! discovered inside rich-text markup — an image the user copied directly or
//! as a file is deliberate and is never filtered.
//!
//! Two stages: [`prefilter`] on the markup attributes, before any bytes are
//! fetched, and [`assess_pixels`] on decoded pixels. [`select_top`] then caps
//! how many survivors are sent, largest first, in document order.

/// Display size (an HTML `width`/`height` attribute) below which an image is
/// dropped before fetching. Loose on purpose: attributes give the display
/// size, not the intrinsic one — a 200px thumbnail may resolve to a 2000px
/// original, and Retina markup halves the real size.
pub const MIN_DISPLAY_EDGE_PX: u32 = 48;
/// Decoded short edge below which nothing legible survives a vision model's
/// internal downscale (Claude documents degradation under 200px per edge).
pub const MIN_SHORT_EDGE_PX: u32 = 128;
/// Decoded area floor (about 200x200), combined with the short-edge floor so a
/// wide banner chart is kept while icons and avatars are not.
pub const MIN_AREA_PX: u64 = 40_000;
/// Long/short edge ratio above which an image is a spacer or a rule.
pub const MAX_ASPECT_RATIO: u32 = 20;
/// Per-channel value range (out of 255) under which sampled pixels count as
/// one flat color: a placeholder box, a spacer, a fully transparent frame.
pub const UNIFORM_RANGE: u8 = 10;
/// Images sent per request; the largest survive.
pub const MAX_IMAGES: usize = 4;

/// Attribute tokens that mark decoration: emoji, icons, avatars, spacers,
/// tracking pixels, logos. Matched case-insensitively as substrings of the
/// `src` path, `class`, and `alt`.
const DECORATIVE_TOKENS: &[&str] = &[
    "emoji", "icon", "avatar", "spacer", "tracking", "pixel", "logo", "bullet", "smiley", "sprite",
];

/// Why an image was left out. Surfaced in logs and the source badge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    /// Display or decoded size under the floor.
    TooSmall { width: u32, height: u32 },
    /// Long/short edge ratio over [`MAX_ASPECT_RATIO`].
    Elongated,
    /// All sampled pixels within [`UNIFORM_RANGE`] per channel.
    Uniform,
    /// A decoration token found in an attribute (the token).
    Decorative(&'static str),
    /// SVG (vector) source: not rasterizable here, and nearly always an icon.
    Vector,
}

/// What markup says about an `<img>` before its bytes are fetched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImgHint<'a> {
    pub src: &'a str,
    /// `width` attribute (or inline style width), in CSS pixels.
    pub width: Option<u32>,
    /// `height` attribute (or inline style height), in CSS pixels.
    pub height: Option<u32>,
    pub alt: Option<&'a str>,
    pub class: Option<&'a str>,
}

/// Stage 1: decide from markup alone. `Ok` means "worth fetching"; a missing
/// dimension never rejects, since only decoded pixels settle it.
pub fn prefilter(hint: &ImgHint) -> Result<(), Rejection> {
    let too_small = |v: Option<u32>| v.is_some_and(|px| px < MIN_DISPLAY_EDGE_PX);
    if too_small(hint.width) || too_small(hint.height) {
        return Err(Rejection::TooSmall {
            width: hint.width.unwrap_or(0),
            height: hint.height.unwrap_or(0),
        });
    }
    if is_vector(hint.src) {
        return Err(Rejection::Vector);
    }
    if let Some(token) = decorative_token(hint) {
        return Err(Rejection::Decorative(token));
    }
    Ok(())
}

/// The path component of `src`, lowercased: no scheme, host, query, or
/// fragment, so a host name or tracking parameter never trips a token match.
/// `None` for `data:` URIs, whose payload is not a path.
fn src_path(src: &str) -> Option<String> {
    let src = src.trim();
    if src.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("data:")) {
        return None;
    }
    let after_scheme = match src.find("://") {
        Some(i) => src[i + 3..].find('/').map_or("", |j| &src[i + 3 + j..]),
        None => src,
    };
    let end = after_scheme.find(['?', '#']).unwrap_or(after_scheme.len());
    Some(after_scheme[..end].to_ascii_lowercase())
}

fn is_vector(src: &str) -> bool {
    if src.trim().get(..14).is_some_and(|p| p.eq_ignore_ascii_case("data:image/svg")) {
        return true;
    }
    src_path(src).is_some_and(|p| p.ends_with(".svg"))
}

fn decorative_token(hint: &ImgHint) -> Option<&'static str> {
    let mut haystacks: Vec<String> = Vec::with_capacity(3);
    if let Some(path) = src_path(hint.src) {
        haystacks.push(path);
    }
    haystacks.extend(hint.class.iter().chain(hint.alt.iter()).map(|s| s.to_ascii_lowercase()));
    DECORATIVE_TOKENS
        .iter()
        .copied()
        .find(|token| haystacks.iter().any(|h| h.contains(token)))
}

/// Stage 2: decide from decoded 8-bit RGBA pixels (`rgba.len() == w*h*4`).
pub fn assess_pixels(rgba: &[u8], width: u32, height: u32) -> Result<(), Rejection> {
    let too_small = Err(Rejection::TooSmall { width, height });
    let expected_len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4));
    if width == 0 || height == 0 || expected_len != Some(rgba.len()) {
        return too_small;
    }
    let (short, long) = (width.min(height), width.max(height));
    if short < MIN_SHORT_EDGE_PX || u64::from(width) * u64::from(height) < MIN_AREA_PX {
        return too_small;
    }
    if u64::from(long) > u64::from(short) * u64::from(MAX_ASPECT_RATIO) {
        return Err(Rejection::Elongated);
    }
    if is_uniform(rgba, width, height) {
        return Err(Rejection::Uniform);
    }
    Ok(())
}

/// Sample a grid of at most 64x64 pixels; flat means every RGBA channel stays
/// within [`UNIFORM_RANGE`] across the samples.
fn is_uniform(rgba: &[u8], width: u32, height: u32) -> bool {
    let step_x = (width / 64).max(1) as usize;
    let step_y = (height / 64).max(1) as usize;
    let mut lo = [u8::MAX; 4];
    let mut hi = [u8::MIN; 4];
    for y in (0..height as usize).step_by(step_y) {
        for x in (0..width as usize).step_by(step_x) {
            let i = (y * width as usize + x) * 4;
            for c in 0..4 {
                lo[c] = lo[c].min(rgba[i + c]);
                hi[c] = hi[c].max(rgba[i + c]);
            }
        }
    }
    (0..4).all(|c| hi[c] - lo[c] < UNIFORM_RANGE)
}

/// Keep at most `max` items, preferring the largest `area`, and return the
/// survivors in their original (document) order.
pub fn select_top<T>(items: Vec<(T, u64)>, max: usize) -> Vec<T> {
    let mut by_area: Vec<usize> = (0..items.len()).collect();
    by_area.sort_by(|&a, &b| items[b].1.cmp(&items[a].1).then(a.cmp(&b)));
    let mut keep = vec![false; items.len()];
    for &i in by_area.iter().take(max) {
        keep[i] = true;
    }
    items
        .into_iter()
        .zip(keep)
        .filter_map(|((item, _), kept)| kept.then_some(item))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, px: [u8; 4]) -> Vec<u8> {
        px.iter().copied().cycle().take((w * h * 4) as usize).collect()
    }

    /// Solid white with a black rectangle in the middle.
    fn with_black_box(w: u32, h: u32) -> Vec<u8> {
        let mut buf = solid(w, h, [255, 255, 255, 255]);
        for y in h / 4..h / 2 {
            for x in w / 4..w / 2 {
                let i = ((y * w + x) * 4) as usize;
                buf[i..i + 3].copy_from_slice(&[0, 0, 0]);
            }
        }
        buf
    }

    fn hint(src: &str) -> ImgHint<'_> {
        ImgHint { src, ..Default::default() }
    }

    #[test]
    fn prefilter_rejects_tracking_pixel_and_tiny_display_size() {
        let px = ImgHint { width: Some(1), height: Some(1), ..hint("https://t.example/p.gif") };
        assert_eq!(prefilter(&px), Err(Rejection::TooSmall { width: 1, height: 1 }));
        let small = ImgHint { width: Some(32), height: None, ..hint("https://h/a.png") };
        assert!(matches!(prefilter(&small), Err(Rejection::TooSmall { .. })));
        let tall_thin = ImgHint { width: Some(400), height: Some(20), ..hint("https://h/a.png") };
        assert!(matches!(prefilter(&tall_thin), Err(Rejection::TooSmall { .. })));
    }

    #[test]
    fn prefilter_keeps_missing_dimensions_and_normal_sizes() {
        assert_eq!(prefilter(&hint("https://h/chart.png")), Ok(()));
        let sized = ImgHint { width: Some(640), height: Some(480), ..hint("https://h/chart.png") };
        assert_eq!(prefilter(&sized), Ok(()));
        let one_side = ImgHint { width: Some(640), height: None, ..hint("https://h/chart.png") };
        assert_eq!(prefilter(&one_side), Ok(()));
    }

    #[test]
    fn prefilter_rejects_decorative_hints() {
        assert_eq!(prefilter(&hint("https://h/img/emoji/1f600.png")), Err(Rejection::Decorative("emoji")));
        let cls = ImgHint { class: Some("user-avatar small"), ..hint("https://h/u/42.jpg") };
        assert_eq!(prefilter(&cls), Err(Rejection::Decorative("avatar")));
        let alt = ImgHint { alt: Some("Company Logo"), ..hint("https://h/u/42.jpg") };
        assert_eq!(prefilter(&alt), Err(Rejection::Decorative("logo")));
        // Only the path is inspected, never the query string or host.
        assert_eq!(prefilter(&hint("https://icon.example/photos/beach.jpg?ref=logo")), Ok(()));
    }

    #[test]
    fn prefilter_survives_multibyte_sources() {
        // A byte-indexed scheme check would slice inside the second Hangul
        // syllable here and panic.
        assert_eq!(prefilter(&hint("한글경로/그림.png")), Ok(()));
        assert_eq!(prefilter(&hint("한글경로/아이콘.svg")), Err(Rejection::Vector));
        assert_eq!(prefilter(&hint("데이터:x")), Ok(()));
    }

    #[test]
    fn prefilter_rejects_vector_sources() {
        assert_eq!(prefilter(&hint("data:image/svg+xml;base64,PHN2Zz4=")), Err(Rejection::Vector));
        assert_eq!(prefilter(&hint("https://h/diagram.SVG?v=2")), Err(Rejection::Vector));
        assert_eq!(prefilter(&hint("data:image/png;base64,iVBORw0KGgo=")), Ok(()));
    }

    #[test]
    fn pixels_reject_small_area_or_short_edge() {
        assert_eq!(
            assess_pixels(&with_black_box(100, 100), 100, 100),
            Err(Rejection::TooSmall { width: 100, height: 100 })
        );
        // 128x128 clears the short edge but not the area floor.
        assert!(matches!(assess_pixels(&with_black_box(128, 128), 128, 128), Err(Rejection::TooSmall { .. })));
        // Wide banner: short edge 100 fails even though the area is large.
        assert!(matches!(assess_pixels(&with_black_box(1500, 100), 1500, 100), Err(Rejection::TooSmall { .. })));
    }

    #[test]
    fn pixels_keep_charts_and_banners() {
        assert_eq!(assess_pixels(&with_black_box(200, 200), 200, 200), Ok(()));
        assert_eq!(assess_pixels(&with_black_box(1000, 150), 1000, 150), Ok(()));
    }

    #[test]
    fn pixels_reject_elongated_rules() {
        assert_eq!(assess_pixels(&with_black_box(4000, 180), 4000, 180), Err(Rejection::Elongated));
    }

    #[test]
    fn pixels_reject_flat_color_and_transparent_frames() {
        assert_eq!(assess_pixels(&solid(300, 300, [255, 255, 255, 255]), 300, 300), Err(Rejection::Uniform));
        assert_eq!(assess_pixels(&solid(300, 300, [0, 0, 0, 0]), 300, 300), Err(Rejection::Uniform));
        // A faint gradient (range under UNIFORM_RANGE) is still flat.
        let mut faint = solid(300, 300, [200, 200, 200, 255]);
        for (i, px) in faint.chunks_mut(4).enumerate() {
            px[0] = 200 + (i % 5) as u8;
        }
        assert_eq!(assess_pixels(&faint, 300, 300), Err(Rejection::Uniform));
    }

    #[test]
    fn pixels_reject_malformed_buffer() {
        assert!(matches!(assess_pixels(&[0u8; 12], 300, 300), Err(Rejection::TooSmall { .. })));
        assert!(matches!(assess_pixels(&[], 0, 0), Err(Rejection::TooSmall { .. })));
    }

    #[test]
    fn select_top_keeps_largest_in_document_order() {
        let items = vec![("a", 10), ("b", 500), ("c", 40), ("d", 300), ("e", 200), ("f", 250)];
        assert_eq!(select_top(items, 4), vec!["b", "d", "e", "f"]);
        let items = vec![("a", 10), ("b", 20)];
        assert_eq!(select_top(items, 4), vec!["a", "b"]);
        assert_eq!(select_top(Vec::<(&str, u64)>::new(), 4), Vec::<&str>::new());
        let items = vec![("a", 10), ("b", 20)];
        assert_eq!(select_top(items, 0), Vec::<&str>::new());
    }
}
