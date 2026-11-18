//! Decide how to treat a fetched body. The HTML extractor only makes sense
//! for HTML; running it over a JSON API response, a raw `.txt`, or a Markdown
//! file would mangle or drop the content. We classify by `Content-Type` when
//! present, and sniff the body otherwise.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Media {
    Html,
    Json,
    Text,
    /// Anything we don't render (binary, PDF, images): the label is the
    /// content-type, surfaced to the caller.
    Other(String),
}

impl Media {
    /// A short, stable label for the `media` field of a result.
    pub fn label(&self) -> String {
        match self {
            Media::Html => "html".into(),
            Media::Json => "json".into(),
            Media::Text => "text".into(),
            Media::Other(ct) => ct.clone(),
        }
    }
}

/// Classify a body using its `Content-Type` header if available, else by
/// sniffing the first non-whitespace bytes. A body that looks binary is
/// `Other` whatever its declared type, so raw bytes are never rendered as text.
pub fn classify(content_type: Option<&str>, body: &str) -> Media {
    let media = classify_declared(content_type, body);
    match media {
        Media::Other(_) => media,
        _ if looks_binary(body) => {
            Media::Other(essence(content_type).unwrap_or_else(|| "application/octet-stream".into()))
        }
        _ => media,
    }
}

fn essence(content_type: Option<&str>) -> Option<String> {
    let essence = content_type?
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    (!essence.is_empty()).then_some(essence)
}

fn classify_declared(content_type: Option<&str>, body: &str) -> Media {
    if let Some(essence) = essence(content_type) {
        if essence.contains("html") || essence == "application/xhtml+xml" {
            return Media::Html;
        }
        if essence.contains("json") {
            return Media::Json;
        }
        if essence.starts_with("text/") {
            return Media::Text;
        }
        return Media::Other(essence);
    }
    sniff(body)
}

/// Does a decoded body look like binary data rather than text? Among the first
/// 8 KiB characters, more than one in ten being a C0 control character (other
/// than the whitespace and escape text uses) counts as binary.
///
/// U+FFFD does not count. A legacy-charset page with no charset label is
/// decoded lossily as UTF-8 and comes out full of replacement characters, and
/// it is still text. Compressed or raw bytes always carry NUL and other C0
/// bytes, so the control-character count is enough to catch them.
pub fn looks_binary(body: &str) -> bool {
    const SAMPLE: usize = 8 * 1024;
    let mut sampled = 0usize;
    let mut odd = 0usize;
    for c in body.chars().take(SAMPLE) {
        sampled += 1;
        let control = c < ' ' && !matches!(c, '\t' | '\n' | '\r' | '\u{0C}' | '\u{1B}');
        if control {
            odd += 1;
        }
    }
    odd * 10 > sampled
}

fn sniff(body: &str) -> Media {
    let trimmed = body.trim_start();
    match trimmed.as_bytes().first() {
        Some(b'<') => Media::Html,
        Some(b'{') | Some(b'[') => {
            if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
                Media::Json
            } else {
                Media::Text
            }
        }
        _ => Media::Text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first bytes of a gzip member, decoded as UTF-8 the way the fetch
    /// path does (`from_utf8_lossy`): what a gzip body looks like when a
    /// transport hands it over undecoded.
    fn raw_gzip_as_text() -> String {
        let bytes: &[u8] = b"\x1f\x8b\x08\x00\x00\x00\x00\x00\x00\x03\xed\x9d\x4b\x8e\xe3\x36\x10\x86\xef\x7e\x85\x9c\x0b\xd2\xf5\x7a\x8a\x45\x3f\x89";
        String::from_utf8_lossy(bytes).into_owned()
    }

    #[test]
    fn binary_body_is_not_rendered_even_under_a_text_type() {
        let body = raw_gzip_as_text();
        assert!(looks_binary(&body));
        assert_eq!(
            classify(Some("application/json"), &body),
            Media::Other("application/json".into())
        );
        assert_eq!(
            classify(None, &body),
            Media::Other("application/octet-stream".into())
        );
    }

    #[test]
    fn ordinary_text_is_unaffected() {
        let json = "{\n\t\"ok\": true\r\n}\n";
        assert!(!looks_binary(json));
        assert_eq!(classify(Some("application/json"), json), Media::Json);
        assert_eq!(classify(Some("text/plain"), "a\x1bb"), Media::Text);
        assert_eq!(classify(None, "<html></html>"), Media::Html);
    }

    #[test]
    fn a_few_stray_bytes_do_not_make_text_binary() {
        let mut text = "word ".repeat(400);
        text.push('\u{FFFD}');
        assert!(!looks_binary(&text));
    }

    /// Windows-1251 Cyrillic with no charset label, as raw bytes. Decoding it
    /// as UTF-8 yields a replacement character per high byte, which is a lot
    /// of U+FFFD, but the page is still text and must render.
    #[test]
    fn unlabelled_windows_1251_html_is_not_binary() {
        // "Privet, mir! Eto testovaya stranitsa." in windows-1251, as raw bytes.
        let line: &[u8] = b"<p>\xcf\xf0\xe8\xe2\xe5\xf2, \xec\xe8\xf0! \xdd\xf2\xee \xf2\xe5\xf1\xf2\xee\xe2\xe0\xff \xf1\xf2\xf0\xe0\xed\xe8\xf6\xe0.</p>";
        let mut bytes = b"<html><body>".to_vec();
        for _ in 0..5 {
            bytes.extend_from_slice(line);
        }
        bytes.extend_from_slice(b"</body></html>");
        let body = String::from_utf8_lossy(&bytes).into_owned();
        assert!(body.matches('\u{FFFD}').count() > body.len() / 10);
        assert!(!looks_binary(&body));
        assert_eq!(classify(Some("text/html"), &body), Media::Html);
        assert_eq!(classify(None, &body), Media::Html);
    }

    /// A gzip body declared as JSON is binary, not a document to print.
    #[test]
    fn gzip_body_declared_json_is_binary() {
        let body = raw_gzip_as_text();
        assert_eq!(
            classify(Some("application/json"), &body),
            Media::Other("application/json".into())
        );
    }
}
