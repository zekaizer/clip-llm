//! Upload encoding shared by every image input path: bound the pixel size,
//! then PNG-encode. One place, so the clipboard image, PNG files, and
//! markup-embedded images obey the same payload bound.

use tracing::{debug, info};

use crate::ClipboardError;

/// Long edge (px) above which a clipboard image is downscaled before PNG
/// encoding. 1568px is a common vision-model sweet spot (e.g. Anthropic/OpenAI
/// vision APIs internally cap around this size); sending anything larger only
/// inflates the base64 payload without improving model accuracy.
const MAX_IMAGE_LONG_EDGE: u32 = 1568;

/// Pixel budget an input image may have before decoding (about 8K x 6K).
/// Checked from the header, so the RGBA buffer for a larger image is never
/// allocated; the file path also caps the decoder's allocation at this many
/// RGBA bytes.
pub const MAX_IMAGE_PIXELS: u64 = 50_000_000;

/// Refuse an image whose header exceeds [`MAX_IMAGE_PIXELS`].
pub fn check_pixel_budget(width: u32, height: u32) -> Result<(), ClipboardError> {
    if u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS {
        return Err(ClipboardError::ImageTooLarge {
            width,
            height,
            limit_px: MAX_IMAGE_PIXELS,
        });
    }
    Ok(())
}


/// Per-channel tolerance for a pixel to count as the margin color.
const TRIM_TOLERANCE: u8 = 8;
/// Margin kept around the content after a trim, so edge glyphs are not cut.
const TRIM_PADDING: u32 = 8;

/// Crop away the flat-colored margin around the content. The margin color is
/// the one most corners share (within [`TRIM_TOLERANCE`]), so content touching
/// one corner does not pass for background. `None` when there is nothing to
/// gain: no margin, a fully flat image, or a crop under 5% of the area.
pub fn trim_uniform_margins(rgba: &[u8], width: u32, height: u32) -> Option<(Vec<u8>, u32, u32)> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || rgba.len() != w * h * 4 {
        return None;
    }
    let bg = margin_color(rgba, w, h)?;
    let is_bg = |i: usize| (0..4).all(|c| rgba[i + c].abs_diff(bg[c]) <= TRIM_TOLERANCE);
    let row_has_content = |y: usize| (0..w).any(|x| !is_bg((y * w + x) * 4));
    let col_has_content = |x: usize, rows: std::ops::Range<usize>| rows.into_iter().any(|y| !is_bg((y * w + x) * 4));

    let top = (0..h).find(|&y| row_has_content(y))?;
    let bottom = (top..h).rev().find(|&y| row_has_content(y))?;
    let left = (0..w).find(|&x| col_has_content(x, top..bottom + 1))?;
    let right = (left..w).rev().find(|&x| col_has_content(x, top..bottom + 1))?;

    let pad = TRIM_PADDING as usize;
    let (x0, y0) = (left.saturating_sub(pad), top.saturating_sub(pad));
    let (x1, y1) = ((right + 1 + pad).min(w), (bottom + 1 + pad).min(h));
    let (cw, ch) = (x1 - x0, y1 - y0);
    if cw * ch * 100 > w * h * 95 {
        return None;
    }
    let mut out = Vec::with_capacity(cw * ch * 4);
    for y in y0..y1 {
        out.extend_from_slice(&rgba[(y * w + x0) * 4..(y * w + x1) * 4]);
    }
    Some((out, cw as u32, ch as u32))
}

/// The color shared by the most corners, within [`TRIM_TOLERANCE`]; `None`
/// when no two corners agree (no flat frame to trim).
fn margin_color(rgba: &[u8], w: usize, h: usize) -> Option<[u8; 4]> {
    let corner = |x: usize, y: usize| -> [u8; 4] {
        let i = (y * w + x) * 4;
        [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
    };
    let corners = [corner(0, 0), corner(w - 1, 0), corner(0, h - 1), corner(w - 1, h - 1)];
    let close = |a: &[u8; 4], b: &[u8; 4]| (0..4).all(|c| a[c].abs_diff(b[c]) <= TRIM_TOLERANCE);
    corners
        .iter()
        .map(|c| (c, corners.iter().filter(|o| close(c, o)).count()))
        .filter(|&(_, n)| n >= 2)
        .max_by_key(|&(_, n)| n)
        .map(|(c, _)| *c)
}

/// Downscale (long edge > `MAX_IMAGE_LONG_EDGE`) and PNG-encode raw RGBA pixels
/// for the vision API. Shared by the clipboard image and PNG-file paths so both
/// obey the same payload bound.
pub fn encode_rgba_for_upload(
    rgba: Vec<u8>,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, ClipboardError> {
    // Only an image that would be downscaled gains from a trim: the margin
    // it sheds is resolution the content keeps.
    let (rgba, width, height) = if width.max(height) > MAX_IMAGE_LONG_EDGE {
        match trim_uniform_margins(&rgba, width, height) {
            Some((cropped, w, h)) => {
                debug!("trimmed flat margins {}x{} -> {}x{}", width, height, w, h);
                (cropped, w, h)
            }
            None => (rgba, width, height),
        }
    } else {
        (rgba, width, height)
    };
    let (bytes, w, h) = match downscale_rgba(&rgba, width, height) {
        Some((resized, new_w, new_h)) => {
            debug!("downscaling image {}x{} -> {}x{}", width, height, new_w, new_h);
            (resized, new_w, new_h)
        }
        None => (rgba, width, height),
    };
    let png = rgba_to_png(&bytes, w, h)?;
    info!("encoded image ({}x{}, {} bytes PNG)", w, h, png.len());
    Ok(png)
}

/// Downscale an RGBA image so its long edge is at most `MAX_IMAGE_LONG_EDGE`,
/// preserving aspect ratio. Returns `None` if the image is already within
/// bounds (no-op, avoids copying the buffer). Uses a hand-rolled box (area
/// average) filter rather than pulling in an image-processing crate, per the
/// project's single-binary / minimal-deps constraint.
fn downscale_rgba(bytes: &[u8], width: u32, height: u32) -> Option<(Vec<u8>, u32, u32)> {
    if width == 0 || height == 0 {
        return None;
    }
    let long_edge = width.max(height);
    if long_edge <= MAX_IMAGE_LONG_EDGE {
        return None;
    }

    let scale = MAX_IMAGE_LONG_EDGE as f64 / long_edge as f64;
    let new_width = ((width as f64 * scale).round() as u32).max(1);
    let new_height = ((height as f64 * scale).round() as u32).max(1);

    Some((
        box_resample(bytes, width, height, new_width, new_height),
        new_width,
        new_height,
    ))
}

/// Box-filter (area average) resample of an RGBA buffer from `src_w x src_h`
/// down to `dst_w x dst_h`. Each destination pixel is the average of the
/// source pixels its area covers, which avoids the aliasing a naive
/// nearest-neighbor downscale would introduce.
fn box_resample(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    let mut dst = vec![0u8; (dst_w as usize) * (dst_h as usize) * 4];
    let x_ratio = src_w as f64 / dst_w as f64;
    let y_ratio = src_h as f64 / dst_h as f64;

    for dy in 0..dst_h {
        let sy0 = (dy as f64 * y_ratio).floor() as u32;
        let sy1 = (((dy + 1) as f64 * y_ratio).ceil() as u32)
            .min(src_h)
            .max(sy0 + 1);
        for dx in 0..dst_w {
            let sx0 = (dx as f64 * x_ratio).floor() as u32;
            let sx1 = (((dx + 1) as f64 * x_ratio).ceil() as u32)
                .min(src_w)
                .max(sx0 + 1);

            let mut r_sum: u64 = 0;
            let mut g_sum: u64 = 0;
            let mut b_sum: u64 = 0;
            let mut a_sum: u64 = 0;
            let mut count: u64 = 0;

            for sy in sy0..sy1 {
                for sx in sx0..sx1 {
                    let idx = ((sy * src_w + sx) * 4) as usize;
                    r_sum += src[idx] as u64;
                    g_sum += src[idx + 1] as u64;
                    b_sum += src[idx + 2] as u64;
                    a_sum += src[idx + 3] as u64;
                    count += 1;
                }
            }

            let didx = ((dy * dst_w + dx) * 4) as usize;
            dst[didx] = (r_sum / count) as u8;
            dst[didx + 1] = (g_sum / count) as u8;
            dst[didx + 2] = (b_sum / count) as u8;
            dst[didx + 3] = (a_sum / count) as u8;
        }
    }

    dst
}

/// Encode raw RGBA pixel data to PNG.
pub fn rgba_to_png(bytes: &[u8], width: u32, height: u32) -> Result<Vec<u8>, ClipboardError> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| ClipboardError::ImageEncodeFailed(e.to_string()))?;
        writer
            .write_image_data(bytes)
            .map_err(|e| ClipboardError::ImageEncodeFailed(e.to_string()))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PNG IHDR dimensions, straight from the header bytes.
    fn png_dims(png: &[u8]) -> (u32, u32) {
        let w = u32::from_be_bytes(png[16..20].try_into().unwrap());
        let h = u32::from_be_bytes(png[20..24].try_into().unwrap());
        (w, h)
    }

    /// `w x h` of `bg` with a block of `fg` at `rect` = (x, y, width, height).
    fn with_box(w: u32, h: u32, bg: [u8; 4], rect: (u32, u32, u32, u32), fg: [u8; 4]) -> Vec<u8> {
        let (x, y, box_w, box_h) = rect;
        let mut buf = solid_rgba(w, h, bg);
        for yy in y..y + box_h {
            for xx in x..x + box_w {
                let i = ((yy * w + xx) * 4) as usize;
                buf[i..i + 4].copy_from_slice(&fg);
            }
        }
        buf
    }

    #[test]
    fn trim_crops_to_content_plus_padding() {
        let img = with_box(400, 300, [255, 255, 255, 255], (100, 50, 100, 100), [0, 0, 0, 255]);
        let (cropped, w, h) = trim_uniform_margins(&img, 400, 300).unwrap();
        assert_eq!((w, h), (100 + 2 * TRIM_PADDING, 100 + 2 * TRIM_PADDING));
        assert_eq!(cropped.len(), (w * h * 4) as usize);
        // Padding ring is background, the inner block is content.
        assert_eq!(&cropped[..4], &[255, 255, 255, 255]);
        let center = (((h / 2) * w + w / 2) * 4) as usize;
        assert_eq!(&cropped[center..center + 4], &[0, 0, 0, 255]);
    }

    #[test]
    fn trim_padding_is_clamped_at_the_image_edge() {
        // Content touches the top-left corner: nothing to pad on that side.
        let img = with_box(300, 300, [20, 20, 20, 255], (0, 0, 50, 50), [200, 200, 200, 255]);
        let (_, w, h) = trim_uniform_margins(&img, 300, 300).unwrap();
        assert_eq!((w, h), (50 + TRIM_PADDING, 50 + TRIM_PADDING));
    }

    #[test]
    fn trim_tolerates_near_background_noise() {
        let mut img = with_box(400, 300, [250, 250, 250, 255], (100, 50, 100, 100), [0, 0, 0, 255]);
        // Faint speckle in the margin, within tolerance.
        img[4 * 10] = 245;
        img[4 * (400 * 200 + 390)] = 255;
        let (_, w, h) = trim_uniform_margins(&img, 400, 300).unwrap();
        assert_eq!((w, h), (100 + 2 * TRIM_PADDING, 100 + 2 * TRIM_PADDING));
    }

    #[test]
    fn trim_is_noop_without_a_worthwhile_margin() {
        // Flat image: nothing is content.
        assert!(trim_uniform_margins(&solid_rgba(300, 300, [9, 9, 9, 255]), 300, 300).is_none());
        // Content fills the frame.
        let full = with_box(300, 300, [255, 255, 255, 255], (0, 0, 300, 300), [0, 0, 0, 255]);
        assert!(trim_uniform_margins(&full, 300, 300).is_none());
        // A 2px margin is under the 5% threshold.
        let thin = with_box(300, 300, [255, 255, 255, 255], (2, 2, 296, 296), [0, 0, 0, 255]);
        assert!(trim_uniform_margins(&thin, 300, 300).is_none());
        assert!(trim_uniform_margins(&[], 0, 0).is_none());
    }

    #[test]
    fn upload_trims_margins_before_deciding_to_downscale() {
        // 4000x3000 mostly white, 1000x800 of content: after the trim the long
        // edge is under the cap, so the content is sent at full resolution.
        let img = with_box(4000, 3000, [255, 255, 255, 255], (1500, 1100, 1000, 800), [0, 0, 0, 255]);
        let png = encode_rgba_for_upload(img, 4000, 3000).unwrap();
        assert_eq!(png_dims(&png), (1000 + 2 * TRIM_PADDING, 800 + 2 * TRIM_PADDING));
    }

    #[test]
    fn upload_leaves_small_images_untouched() {
        // Under the cap: no trim, no downscale, even with a wide margin.
        let img = with_box(400, 300, [255, 255, 255, 255], (100, 50, 100, 100), [0, 0, 0, 255]);
        let png = encode_rgba_for_upload(img, 400, 300).unwrap();
        assert_eq!(png_dims(&png), (400, 300));
    }

    #[test]
    fn pixel_budget_admits_up_to_the_limit() {
        assert!(check_pixel_budget(10_000, 5_000).is_ok());
        assert!(check_pixel_budget(1, 1).is_ok());
    }

    #[test]
    fn pixel_budget_refuses_oversized_and_never_overflows() {
        assert!(matches!(
            check_pixel_budget(10_000, 5_001),
            Err(ClipboardError::ImageTooLarge { width: 10_000, height: 5_001, limit_px: MAX_IMAGE_PIXELS })
        ));
        assert!(matches!(check_pixel_budget(u32::MAX, u32::MAX), Err(ClipboardError::ImageTooLarge { .. })));
    }

    // -- rgba_to_png tests --

    #[test]
    fn rgba_to_png_valid_data() {
        // 2x2 RGBA pixels (16 bytes)
        let pixels: Vec<u8> = vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];
        let png = rgba_to_png(&pixels, 2, 2).unwrap();

        // PNG signature: 0x89 P N G
        assert!(png.len() > 8);
        assert_eq!(&png[..4], &[0x89, 0x50, 0x4E, 0x47]);
    }

    #[test]
    fn rgba_to_png_invalid_dimensions() {
        // 3 bytes is not enough for any valid RGBA image
        let result = rgba_to_png(&[0, 0, 0], 2, 2);
        assert!(matches!(result, Err(ClipboardError::ImageEncodeFailed(_))));
    }

    // -- downscale_rgba tests --

    /// Build a solid-color RGBA buffer for testing (cheap, no need for varied pixels).
    fn solid_rgba(width: u32, height: u32, color: [u8; 4]) -> Vec<u8> {
        let mut buf = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..(width * height) {
            buf.extend_from_slice(&color);
        }
        buf
    }

    #[test]
    fn downscale_rgba_noop_for_small_image() {
        let pixels = solid_rgba(100, 50, [10, 20, 30, 255]);
        let result = downscale_rgba(&pixels, 100, 50);
        assert!(result.is_none());
    }

    #[test]
    fn downscale_rgba_noop_at_exact_threshold() {
        // Long edge exactly at the cap must not be resampled.
        let pixels = solid_rgba(MAX_IMAGE_LONG_EDGE, 100, [1, 2, 3, 255]);
        let result = downscale_rgba(&pixels, MAX_IMAGE_LONG_EDGE, 100);
        assert!(result.is_none());
    }

    #[test]
    fn downscale_rgba_scales_down_preserving_aspect_ratio() {
        // 2x oversized square-ish image: long edge should land exactly on the cap.
        let width = MAX_IMAGE_LONG_EDGE * 2;
        let height = MAX_IMAGE_LONG_EDGE;
        let pixels = solid_rgba(width, height, [200, 100, 50, 255]);

        let (resized, new_w, new_h) = downscale_rgba(&pixels, width, height).unwrap();

        assert_eq!(new_w, MAX_IMAGE_LONG_EDGE);
        assert_eq!(new_h, MAX_IMAGE_LONG_EDGE / 2);
        assert_eq!(resized.len(), (new_w * new_h * 4) as usize);

        // Aspect ratio preserved (within rounding).
        let orig_ratio = width as f64 / height as f64;
        let new_ratio = new_w as f64 / new_h as f64;
        assert!((orig_ratio - new_ratio).abs() < 0.01);
    }

    #[test]
    fn downscale_rgba_solid_color_preserved() {
        // A solid-color image should downscale to the same solid color (box
        // filter averaging a uniform region yields that same value).
        let width = MAX_IMAGE_LONG_EDGE * 2;
        let height = MAX_IMAGE_LONG_EDGE;
        let color = [77, 88, 99, 255];
        let pixels = solid_rgba(width, height, color);

        let (resized, new_w, new_h) = downscale_rgba(&pixels, width, height).unwrap();

        for chunk in resized.as_chunks::<4>().0 {
            assert_eq!(*chunk, color);
        }
        assert_eq!(resized.len(), (new_w * new_h * 4) as usize);
    }

    #[test]
    fn downscale_rgba_degenerate_1px_wide() {
        // Extremely tall, 1px-wide image: width must stay clamped to at least 1
        // after scaling, height clamped to the cap.
        let width = 1;
        let height = MAX_IMAGE_LONG_EDGE * 10;
        let pixels = solid_rgba(width, height, [5, 6, 7, 255]);

        let (resized, new_w, new_h) = downscale_rgba(&pixels, width, height).unwrap();

        assert_eq!(new_w, 1);
        assert_eq!(new_h, MAX_IMAGE_LONG_EDGE);
        assert_eq!(resized.len(), (new_w * new_h * 4) as usize);
    }

    #[test]
    fn downscale_rgba_zero_dimension_is_noop() {
        // Degenerate zero-sized image must not panic or divide by zero.
        assert!(downscale_rgba(&[], 0, 0).is_none());
        assert!(downscale_rgba(&[], 0, 100).is_none());
    }
}
