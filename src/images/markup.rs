//! Images referenced from the clipboard's HTML flavor, resolved into
//! attachments: markup hints → fetch → decode → pixel check → encode → cap.
//! Fetching is injected, so the pipeline runs in tests without a clipboard
//! or a network.

use tracing::{debug, warn};

use super::decode::decode_rgba;
use super::encode::encode_for_upload;
use super::filter::{self, MAX_IMAGES};
use super::html::{fragment, img_refs};
use super::{cap_for_request, ImageAttachment, ImageOrigin};

/// Candidates fetched per capture at most. A page can reference hundreds of
/// images; the first ones that pass the markup prefilter are the ones that
/// get bytes, and only [`MAX_IMAGES`] of those are sent.
pub const MAX_FETCHES: usize = MAX_IMAGES * 2;

/// Resolve the `<img>` references in `html` into attachments. `fetch` returns
/// the bytes behind a `src` (`None` when unavailable). A reference that is
/// decorative, unfetchable, undecodable, too small, or over the pixel budget
/// is skipped, never fatal: the text is still worth sending.
pub fn images_from_html(html: &str, fetch: &dyn Fn(&str) -> Option<Vec<u8>>) -> Vec<ImageAttachment> {
    let refs = img_refs(fragment(html));
    let mut images = Vec::new();
    let mut fetched = 0usize;
    for r in &refs {
        if fetched >= MAX_FETCHES {
            debug!("markup: fetch budget reached; {} reference(s) left unread", refs.len() - fetched);
            break;
        }
        let src = short(&r.src);
        if let Err(why) = filter::prefilter(&r.hint()) {
            debug!("markup: skip {src}: {why:?}");
            continue;
        }
        fetched += 1;
        let Some(bytes) = fetch(&r.src) else {
            debug!("markup: {src}: unavailable");
            continue;
        };
        let (rgba, w, h) = match decode_rgba(&bytes) {
            Ok(Some(decoded)) => decoded,
            Ok(None) => {
                debug!("markup: {src}: not a decodable image");
                continue;
            }
            Err(e) => {
                warn!("markup: skipping {src}: {e}");
                continue;
            }
        };
        if let Err(why) = filter::assess_pixels(&rgba, w, h) {
            debug!("markup: skip {src} ({w}x{h}): {why:?}");
            continue;
        }
        match encode_for_upload(rgba, w, h, ImageOrigin::Markup) {
            Ok(att) => images.push(att),
            Err(e) => warn!("markup: skipping {src}: {e}"),
        }
    }
    cap_for_request(images)
}

/// A `src` cut to log width; data URIs would otherwise print their payload.
fn short(src: &str) -> String {
    const MAX: usize = 80;
    match src.char_indices().nth(MAX) {
        Some((i, _)) => format!("{}…", &src[..i]),
        None => src.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::images::ImageOrigin;

    /// A non-flat `w x h` PNG (gradient with a dark block).
    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(w, h, |x, y| {
            if x > w / 4 && x < w / 2 && y > h / 4 && y < h / 2 {
                image::Rgba([0, 0, 0, 255])
            } else {
                image::Rgba([(x % 256) as u8, (y % 256) as u8, 200, 255])
            }
        });
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn keeps_meaningful_images_and_drops_the_rest() {
        let html = "<p>a</p><img src=\"https://h/chart.png\"><img src=\"https://h/emoji/smile.png\">\
                    <img src=\"https://h/tiny.png\"><img src=\"https://h/flat.png\"><img src=\"https://h/photo.jpg\">";
        let calls = Cell::new(0);
        let fetch = |src: &str| {
            calls.set(calls.get() + 1);
            match src {
                "https://h/chart.png" => Some(png(600, 400)),
                "https://h/tiny.png" => Some(png(32, 32)),
                "https://h/flat.png" => {
                    let img = image::RgbaImage::from_pixel(300, 300, image::Rgba([255, 255, 255, 255]));
                    let mut out = std::io::Cursor::new(Vec::new());
                    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
                    Some(out.into_inner())
                }
                "https://h/photo.jpg" => Some(png(500, 500)),
                other => panic!("unexpected fetch of {other}"),
            }
        };
        let images = images_from_html(html, &fetch);
        let sizes: Vec<(u32, u32)> = images.iter().map(|i| (i.width, i.height)).collect();
        assert_eq!(sizes, vec![(600, 400), (500, 500)]);
        assert!(images.iter().all(|i| i.origin == ImageOrigin::Markup));
        // The emoji was rejected from markup alone: never fetched.
        assert_eq!(calls.get(), 4);
    }

    #[test]
    fn unfetchable_undecodable_and_oversized_are_skipped_not_fatal() {
        let html = "<img src=\"a.png\"><img src=\"b.png\"><img src=\"c.png\"><img src=\"d.png\">";
        let fetch = |src: &str| match src {
            "a.png" => None,
            "b.png" => Some(b"not an image".to_vec()),
            "c.png" => Some(crate::files::test_support::png_with_header(20_000, 20_000)),
            "d.png" => Some(png(400, 300)),
            _ => None,
        };
        let images = images_from_html(html, &fetch);
        assert_eq!(images.len(), 1);
        assert_eq!((images[0].width, images[0].height), (400, 300));
    }

    #[test]
    fn cf_html_wrapper_is_unwrapped_first() {
        let html = "Version:0.9\r\nStartHTML:0000000105\r\n<html><body><!--StartFragment--><img src=\"x.png\"><!--EndFragment--></body></html>";
        let images = images_from_html(html, &|_| Some(png(300, 300)));
        assert_eq!(images.len(), 1);
    }

    #[test]
    fn fetches_stop_at_the_budget_and_sends_at_most_max_images() {
        let html: String = (0..30).map(|i| format!("<img src=\"i{i}.png\">")).collect();
        let calls = Cell::new(0);
        let fetch = |_: &str| {
            calls.set(calls.get() + 1);
            Some(png(300, 300))
        };
        let images = images_from_html(&html, &fetch);
        assert_eq!(calls.get(), MAX_FETCHES);
        assert_eq!(images.len(), MAX_IMAGES);
    }

    #[test]
    fn no_images_without_markup_references() {
        assert!(images_from_html("<p>just text</p>", &|_| panic!("no fetch expected")).is_empty());
        assert!(images_from_html("", &|_| None).is_empty());
    }
}
