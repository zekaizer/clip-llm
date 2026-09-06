//! Image handling shared by every input path (clipboard image flavor, file
//! list, HTML-embedded references): deciding which images are worth sending
//! and the shape they are sent in.

use std::sync::Arc;

pub mod decode;
pub mod encode;
pub mod filter;
pub mod html;
pub mod markup;

/// Where an image came from. Markup images go through the meaningfulness
/// filter; a clipboard image or a file is deliberate and never filtered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageOrigin {
    Clipboard,
    File,
    Markup,
}

/// Encoding of an attachment's bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageMime {
    Png,
    Jpeg,
}

impl ImageMime {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
        }
    }
}

/// An image ready for upload, plus what it was before the pipeline touched
/// it, so the request and the UI can say how much was shed.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageAttachment {
    /// Encoded bytes, shared across threads without copying.
    pub bytes: Arc<Vec<u8>>,
    pub mime: ImageMime,
    /// Pixel size of `bytes`.
    pub width: u32,
    pub height: u32,
    /// Pixel size of the source before any trim or downscale.
    pub source_width: u32,
    pub source_height: u32,
    pub origin: ImageOrigin,
}

impl ImageAttachment {
    /// Inline `data:` URI, the form both request schemas take.
    pub fn data_uri(&self) -> String {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(self.bytes.as_slice());
        format!("data:{};base64,{b64}", self.mime.as_str())
    }

    /// Prompt tokens this image costs, by the Claude rule of thumb
    /// (width x height / 750), a conservative bound for the other providers.
    pub fn estimated_tokens(&self) -> u32 {
        let tokens = u64::from(self.width) * u64::from(self.height) / 750;
        u32::try_from(tokens).unwrap_or(u32::MAX).max(1)
    }

    fn area(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    /// Whether the sent size differs from the source size.
    pub fn is_resized(&self) -> bool {
        (self.width, self.height) != (self.source_width, self.source_height)
    }

    /// A 2x2 clipboard PNG stand-in for tests that only route bytes.
    #[cfg(test)]
    pub fn stub(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Arc::new(bytes),
            mime: ImageMime::Png,
            width: 2,
            height: 2,
            source_width: 2,
            source_height: 2,
            origin: ImageOrigin::Clipboard,
        }
    }
}

/// Encoded bytes a request may carry across all images. A 5 MB per-image
/// provider limit and an intranet body cap both sit below what four full
/// PNGs could add up to.
pub const MAX_TOTAL_IMAGE_BYTES: usize = 8 * 1024 * 1024;

/// Bound a request's images: at most [`filter::MAX_IMAGES`], the largest by
/// pixel area, and no more than [`MAX_TOTAL_IMAGE_BYTES`] of encoded bytes,
/// dropping the smallest first when over. Document order is kept.
pub fn cap_for_request(images: Vec<ImageAttachment>) -> Vec<ImageAttachment> {
    let ranked: Vec<(ImageAttachment, u64)> = images.into_iter().map(|i| { let area = i.area(); (i, area) }).collect();
    let mut kept = filter::select_top(ranked, filter::MAX_IMAGES);
    while kept.iter().map(|i| i.bytes.len()).sum::<usize>() > MAX_TOTAL_IMAGE_BYTES {
        let Some((smallest, _)) = kept.iter().enumerate().min_by_key(|(_, i)| i.area()) else {
            break;
        };
        kept.remove(smallest);
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sized(w: u32, h: u32, bytes: usize) -> ImageAttachment {
        ImageAttachment {
            width: w,
            height: h,
            source_width: w,
            source_height: h,
            ..ImageAttachment::stub(vec![0u8; bytes])
        }
    }

    #[test]
    fn estimated_tokens_follow_the_area_rule() {
        assert_eq!(sized(1568, 1176, 0).estimated_tokens(), 1568 * 1176 / 750);
        assert_eq!(sized(200, 200, 0).estimated_tokens(), 53);
        assert_eq!(sized(1, 1, 0).estimated_tokens(), 1, "never free");
    }

    #[test]
    fn cap_keeps_the_largest_images_in_document_order() {
        let images = vec![sized(100, 100, 10), sized(1000, 1000, 10), sized(300, 300, 10), sized(900, 900, 10), sized(500, 500, 10), sized(800, 800, 10)];
        let kept: Vec<u32> = cap_for_request(images).iter().map(|i| i.width).collect();
        assert_eq!(kept, vec![1000, 900, 500, 800]);
    }

    #[test]
    fn cap_sheds_smallest_until_bytes_fit() {
        let mb = 1024 * 1024;
        let images = vec![sized(1000, 1000, 5 * mb), sized(200, 200, 2 * mb), sized(900, 900, 4 * mb)];
        // 11 MB total: the 200px image goes first, then it fits at 9 MB? No —
        // 9 MB still exceeds 8 MB, so the 900px one goes too.
        let kept: Vec<u32> = cap_for_request(images).iter().map(|i| i.width).collect();
        assert_eq!(kept, vec![1000]);
        let images = vec![sized(1000, 1000, 5 * mb), sized(900, 900, 2 * mb)];
        assert_eq!(cap_for_request(images).len(), 2);
        assert!(cap_for_request(Vec::new()).is_empty());
    }

    #[test]
    fn data_uri_carries_the_mime_and_base64_payload() {
        let png = ImageAttachment::stub(vec![0x89, 0x50, 0x4E, 0x47]);
        assert_eq!(png.data_uri(), "data:image/png;base64,iVBORw==");
        let jpeg = ImageAttachment { mime: ImageMime::Jpeg, ..ImageAttachment::stub(vec![0xFF, 0xD8]) };
        assert_eq!(jpeg.data_uri(), "data:image/jpeg;base64,/9g=");
    }

    #[test]
    fn resized_means_sent_size_differs_from_source() {
        let same = ImageAttachment::stub(vec![]);
        assert!(!same.is_resized());
        let shrunk = ImageAttachment { source_width: 4000, source_height: 3000, ..ImageAttachment::stub(vec![]) };
        assert!(shrunk.is_resized());
    }
}
