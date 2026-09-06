//! Decoding any supported raster format (PNG, JPEG, GIF, WebP, BMP) into
//! 8-bit RGBA, with the pixel budget enforced from the header before the
//! frame buffer exists.

use std::io::Cursor;

use image::ImageReader;

use super::encode::{check_pixel_budget, MAX_IMAGE_PIXELS};
use crate::ClipboardError;

/// Decode `bytes` into RGBA. `Ok(None)` when the bytes are not a decodable
/// image in a supported format; `Err` only for an image over the pixel budget.
pub fn decode_rgba(bytes: &[u8]) -> Result<Option<(Vec<u8>, u32, u32)>, ClipboardError> {
    let Ok(mut header) = ImageReader::new(Cursor::new(bytes)).with_guessed_format() else {
        return Ok(None);
    };
    let Some(format) = header.format() else {
        return Ok(None);
    };
    // Dimensions come from the header alone; no limit may reject them before
    // the budget check gets to name the actual size.
    header.limits(image::Limits::no_limits());
    let Ok((width, height)) = header.into_dimensions() else {
        return Ok(None);
    };
    check_pixel_budget(width, height)?;

    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(width);
    limits.max_image_height = Some(height);
    // The budget in RGBA bytes, doubled for a decoder's working buffers.
    limits.max_alloc = Some(MAX_IMAGE_PIXELS * 4 * 2);
    reader.limits(limits);
    let Ok(decoded) = reader.decode() else {
        return Ok(None);
    };
    let rgba = decoded.into_rgba8();
    let (w, h) = rgba.dimensions();
    Ok(Some((rgba.into_raw(), w, h)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, RgbImage, RgbaImage};

    fn rgba_fixture(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x * 7) as u8, (y * 5) as u8, 90, 255]))
    }

    fn encoded(format: ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        match format {
            ImageFormat::Jpeg => {
                let rgb = RgbImage::from_fn(40, 30, |x, y| image::Rgb([(x * 6) as u8, (y * 8) as u8, 120]));
                rgb.write_to(&mut out, format).unwrap();
            }
            _ => rgba_fixture(40, 30).write_to(&mut out, format).unwrap(),
        }
        out.into_inner()
    }

    #[test]
    fn decodes_png_gif_bmp_and_jpeg() {
        for format in [ImageFormat::Png, ImageFormat::Gif, ImageFormat::Bmp, ImageFormat::Jpeg] {
            let (rgba, w, h) = decode_rgba(&encoded(format)).unwrap().unwrap_or_else(|| panic!("{format:?}"));
            assert_eq!((w, h), (40, 30), "{format:?}");
            assert_eq!(rgba.len(), 40 * 30 * 4, "{format:?}");
            assert_eq!(rgba[3], 255, "{format:?}: opaque alpha");
        }
    }

    #[test]
    fn png_alpha_survives() {
        let mut img = rgba_fixture(8, 8);
        img.put_pixel(0, 0, image::Rgba([1, 2, 3, 0]));
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, ImageFormat::Png).unwrap();
        let (rgba, ..) = decode_rgba(&out.into_inner()).unwrap().unwrap();
        assert_eq!(&rgba[..4], &[1, 2, 3, 0]);
    }

    #[test]
    fn garbage_and_svg_are_not_images() {
        assert_eq!(decode_rgba(b"not an image at all").unwrap(), None);
        assert_eq!(decode_rgba(b"<svg xmlns='http://www.w3.org/2000/svg'/>").unwrap(), None);
        assert_eq!(decode_rgba(&[]).unwrap(), None);
    }

    #[test]
    fn oversized_header_is_refused_without_decoding() {
        // A PNG header claiming 20000x20000 with an empty IDAT: the dimensions
        // parse, the budget check fires, nothing is allocated for pixels.
        let png = crate::files::test_support::png_with_header(20_000, 20_000);
        assert!(matches!(
            decode_rgba(&png),
            Err(ClipboardError::ImageTooLarge { width: 20_000, height: 20_000, .. })
        ));
    }
}
