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


/// Downscale (long edge > `MAX_IMAGE_LONG_EDGE`) and PNG-encode raw RGBA pixels
/// for the vision API. Shared by the clipboard image and PNG-file paths so both
/// obey the same payload bound.
pub fn encode_rgba_for_upload(
    rgba: Vec<u8>,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, ClipboardError> {
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
