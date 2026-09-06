//! `<img>` references in the clipboard's HTML flavor (`public.html` on macOS,
//! CF_HTML "HTML Format" on Windows). Pure string scanning: no HTML parser
//! dependency, and only the attributes the image filter needs.

use super::filter::ImgHint;

/// One `<img>` in document order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImgRef {
    /// The `src` value, entity-decoded (`&amp;` → `&`).
    pub src: String,
    /// `width` attribute or inline `style` width, in CSS pixels.
    pub width: Option<u32>,
    /// `height` attribute or inline `style` height, in CSS pixels.
    pub height: Option<u32>,
    pub alt: Option<String>,
    pub class: Option<String>,
    /// Byte offset of the tag within the fragment, for interleaving.
    pub offset: usize,
}

impl ImgRef {
    pub fn hint(&self) -> ImgHint<'_> {
        ImgHint {
            src: &self.src,
            width: self.width,
            height: self.height,
            alt: self.alt.as_deref(),
            class: self.class.as_deref(),
        }
    }
}

/// The markup between CF_HTML's `<!--StartFragment-->` / `<!--EndFragment-->`
/// markers, or the whole input when they are absent (macOS `public.html`, or
/// a CF_HTML reader that already stripped the header).
pub fn fragment(html: &str) -> &str {
    const START: &str = "<!--StartFragment-->";
    const END: &str = "<!--EndFragment-->";
    let Some(start) = html.find(START) else {
        return html;
    };
    let body = &html[start + START.len()..];
    body.find(END).map_or(body, |end| &body[..end])
}

/// Every `<img>` with a `src`, in document order, one per distinct `src`.
pub fn img_refs(html: &str) -> Vec<ImgRef> {
    let mut refs: Vec<ImgRef> = Vec::new();
    let bytes = html.as_bytes();
    let mut pos = 0;
    while let Some(rel) = find_ci(&bytes[pos..], b"<img") {
        let start = pos + rel;
        let after = start + 4;
        // `<image`, `<imgx`: not an image tag.
        if !bytes.get(after).is_some_and(|b| b.is_ascii_whitespace() || *b == b'/' || *b == b'>') {
            pos = after;
            continue;
        }
        let end = tag_end(bytes, after);
        pos = (end + 1).min(bytes.len());
        let attrs = parse_attrs(&html[after..end]);
        let get = |name: &str| attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
        let Some(src) = get("src").map(str::trim).filter(|v| !v.is_empty()) else {
            continue;
        };
        if refs.iter().any(|r| r.src == src) {
            continue;
        }
        let style = get("style");
        refs.push(ImgRef {
            src: src.to_owned(),
            width: get("width").and_then(css_px).or_else(|| style.and_then(|s| style_px(s, "width"))),
            height: get("height").and_then(css_px).or_else(|| style.and_then(|s| style_px(s, "height"))),
            alt: get("alt").map(str::to_owned),
            class: get("class").map(str::to_owned),
            offset: start,
        });
    }
    refs
}

/// Case-insensitive byte search.
fn find_ci(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w.eq_ignore_ascii_case(needle))
}

/// Index of the `>` closing the tag whose attributes start at `from`,
/// skipping quoted values; the input length when unterminated.
fn tag_end(bytes: &[u8], from: usize) -> usize {
    let mut quote: Option<u8> = None;
    for (i, &b) in bytes.iter().enumerate().skip(from) {
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None if b == b'"' || b == b'\'' => quote = Some(b),
            None if b == b'>' => return i,
            None => {}
        }
    }
    bytes.len()
}

/// `name=value` pairs (names lowercased, values entity-decoded), first
/// occurrence wins. Values may be double-quoted, single-quoted, or bare.
fn parse_attrs(attrs: &str) -> Vec<(String, String)> {
    let bytes = attrs.as_bytes();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() || bytes[i] == b'/' {
            i += 1;
            continue;
        }
        let name_start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'=' && bytes[i] != b'/' {
            i += 1;
        }
        let name = attrs[name_start..i].to_ascii_lowercase();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let raw = if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let q = bytes[i];
                let start = i + 1;
                let close = bytes[start..].iter().position(|&b| b == q).map_or(bytes.len(), |n| start + n);
                i = (close + 1).min(bytes.len());
                &attrs[start..close]
            } else {
                let start = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                &attrs[start..i]
            };
            value = decode_entities(raw);
        }
        if !name.is_empty() && !out.iter().any(|(k, _)| *k == name) {
            out.push((name, value));
        }
    }
    out
}

/// Decode the entities that occur in attribute values (`&amp;`, `&quot;`,
/// `&#39;`, `&lt;`, `&gt;`, `&nbsp;`, numeric); anything else stays literal.
fn decode_entities(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_owned();
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        let Some(semi) = tail[..tail.len().min(10)].find(';') else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let entity = &tail[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "nbsp" => Some(' '),
            _ => entity.strip_prefix('#').and_then(|n| {
                let code = match n.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => n.parse().ok(),
                };
                code.and_then(char::from_u32)
            }),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// A CSS pixel length: `640`, `640px`, `320.5px`. Percentages, `auto`, and
/// other units are unknown (`None`).
fn css_px(value: &str) -> Option<u32> {
    let v = value.trim();
    let num = v
        .strip_suffix("px")
        .or_else(|| v.strip_suffix("PX"))
        .unwrap_or(v)
        .trim();
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return None;
    }
    let px = num.parse::<f64>().ok()?.round();
    (px >= 1.0 && px <= f64::from(u32::MAX)).then_some(px as u32)
}

/// `prop` from an inline `style` declaration list, as CSS pixels.
fn style_px(style: &str, prop: &str) -> Option<u32> {
    style.split(';').find_map(|decl| {
        let (k, v) = decl.split_once(':')?;
        k.trim().eq_ignore_ascii_case(prop).then(|| css_px(v)).flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CF_HTML: &str = "Version:0.9\r\nStartHTML:0000000105\r\nEndHTML:0000000310\r\nStartFragment:0000000141\r\nEndFragment:0000000274\r\n<html>\r\n<body>\r\n<!--StartFragment--><p>Hello <img src=\"https://intranet/chart.png\" width=\"640\" height=\"360\" alt=\"Q3 chart\"> world</p><!--EndFragment-->\r\n</body>\r\n</html>";

    const CHROME_MAC: &str = "<meta charset='utf-8'><img src=\"https://h/photo.jpg?x=1&amp;y=2\" style=\"width: 320px; height: 240px;\" class=\"photo\">";

    const WORD: &str = "<p class=MsoNormal><!--[if gte vml 1]><v:shape id=\"Picture_x0020_1\" style='width:468pt;height:234pt'><v:imagedata src=\"file:///C:/Users/U/AppData/Local/Temp/msohtmlclip1/01/clip_image001.png\" o:title=\"\"/></v:shape><![endif]--><![if !vml]><img width=624 height=312 src=\"file:///C:/Users/U/AppData/Local/Temp/msohtmlclip1/01/clip_image002.png\" v:shapes=\"Picture_x0020_1\"><![endif]></p>";

    #[test]
    fn fragment_strips_the_cf_html_wrapper() {
        assert_eq!(
            fragment(CF_HTML),
            "<p>Hello <img src=\"https://intranet/chart.png\" width=\"640\" height=\"360\" alt=\"Q3 chart\"> world</p>"
        );
        assert_eq!(fragment(CHROME_MAC), CHROME_MAC);
        // A start marker without an end marker: take the rest.
        assert_eq!(fragment("x<!--StartFragment-->tail"), "tail");
    }

    #[test]
    fn refs_from_cf_html_carry_attributes() {
        let refs = img_refs(fragment(CF_HTML));
        assert_eq!(refs.len(), 1, "{refs:?}");
        assert_eq!(refs[0].src, "https://intranet/chart.png");
        assert_eq!((refs[0].width, refs[0].height), (Some(640), Some(360)));
        assert_eq!(refs[0].alt.as_deref(), Some("Q3 chart"));
        assert_eq!(refs[0].class, None);
        assert_eq!(refs[0].offset, "<p>Hello ".len());
    }

    #[test]
    fn refs_decode_entities_and_read_inline_style_size() {
        let refs = img_refs(CHROME_MAC);
        assert_eq!(refs.len(), 1, "{refs:?}");
        assert_eq!(refs[0].src, "https://h/photo.jpg?x=1&y=2");
        assert_eq!((refs[0].width, refs[0].height), (Some(320), Some(240)));
        assert_eq!(refs[0].class.as_deref(), Some("photo"));
    }

    #[test]
    fn refs_take_word_img_not_vml_imagedata() {
        let refs = img_refs(WORD);
        assert_eq!(refs.len(), 1, "{refs:?}");
        assert!(refs[0].src.ends_with("clip_image002.png"), "{}", refs[0].src);
        assert_eq!((refs[0].width, refs[0].height), (Some(624), Some(312)));
    }

    #[test]
    fn refs_dedupe_by_src_in_document_order() {
        let html = "<img src='b.png'><IMG SRC=\"a.png\" WIDTH=100><img src='b.png' width=9><img src=\"c.png\"/>";
        let refs = img_refs(html);
        let srcs: Vec<&str> = refs.iter().map(|r| r.src.as_str()).collect();
        assert_eq!(srcs, vec!["b.png", "a.png", "c.png"]);
        assert_eq!(refs[0].width, None);
        assert_eq!(refs[1].width, Some(100));
        assert!(refs[0].offset < refs[1].offset && refs[1].offset < refs[2].offset);
    }

    #[test]
    fn refs_skip_tags_without_src_and_non_pixel_sizes() {
        let html = "<img alt='no source'><img src=\"d.png\" width=\"100%\" height=\"auto\"><img src=\"data:image/png;base64,iVBORw0KGgo=\" />";
        let refs = img_refs(html);
        assert_eq!(refs.len(), 2, "{refs:?}");
        assert_eq!((refs[0].width, refs[0].height), (None, None));
        assert!(refs[1].src.starts_with("data:image/png;base64,"));
    }

    #[test]
    fn refs_attribute_wins_over_style_and_px_suffix_is_accepted() {
        let html = "<img src=\"e.png\" width=\"800px\" style=\"width:100px\">";
        let refs = img_refs(html);
        assert_eq!(refs[0].width, Some(800));
        // `<image` or `<imgx` are not image tags.
        assert!(img_refs("<imgx src='f.png'><image src='g.png'>").is_empty());
    }

    #[test]
    fn hint_borrows_the_ref() {
        let r = ImgRef {
            src: "h.png".into(),
            width: Some(1),
            height: None,
            alt: Some("a".into()),
            class: None,
            offset: 0,
        };
        assert_eq!(
            r.hint(),
            ImgHint { src: "h.png", width: Some(1), height: None, alt: Some("a"), class: None }
        );
    }
}
