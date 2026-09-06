//! Image handling shared by every input path (clipboard image flavor, file
//! list, HTML-embedded references): deciding which images are worth sending
//! and the shape they are sent in.

use std::sync::Arc;

pub mod decode;
pub mod encode;
pub mod filter;
pub mod html;

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

#[cfg(test)]
mod tests {
    use super::*;

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
