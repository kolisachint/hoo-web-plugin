//! webfetch — token-efficient web content fetcher.
//!
//! The defining feature is **reference-style URL preservation**: instead of
//! stripping links to their domain (losing the ability to cite or follow
//! them) or expanding full URLs inline (wasting tokens), links are replaced
//! with compact `[N]` markers and collected into a recoverable reference list.

// Shared primitives live in webfetch-core; re-export them so both this
// crate's internal modules (via `crate::compress` / `crate::refs`) and
// external callers keep a stable path.
#[cfg(feature = "native")]
pub use webfetch_core::tls;
pub use webfetch_core::{charset, compress, http, refs};

pub mod convert;
pub mod extract;
pub mod fetch;
pub mod grep;
#[cfg(feature = "native")]
pub mod guard;
pub mod limits;
pub mod media;
pub mod outline;
pub mod types;

#[cfg(feature = "native")]
pub use fetch::fetch_page;
pub use fetch::fetch_with;
use media::Media;
use types::{
    ContentStatus, ContentType, FetchOptions, FetchResult, Metadata, OptionsError, UrlReference,
};

use scraper::{Html, Selector};

/// Convert already-fetched HTML into a [`FetchResult`] without any network I/O.
///
/// Useful for tests and for callers that obtain HTML by other means. Always
/// treats the input as HTML; use [`convert_body`] for media-aware handling.
pub fn convert_html(
    html: &str,
    source_url: &str,
    options: &FetchOptions,
) -> Result<FetchResult, OptionsError> {
    convert_body(html, source_url, Some("text/html"), options)
}

/// Convert a fetched body to a [`FetchResult`], choosing how to treat it based
/// on its `Content-Type` (or a sniff of the body). HTML is extracted; JSON is
/// pretty-printed; other text is passed through verbatim; binary is summarized.
///
/// Fails when the options cannot be honoured: see [`FetchOptions::validate`],
/// and an `offset` at or past the end of the converted text.
pub fn convert_body(
    body: &str,
    source_url: &str,
    content_type_header: Option<&str>,
    options: &FetchOptions,
) -> Result<FetchResult, OptionsError> {
    options.validate()?;
    let media = media::classify(content_type_header, body);

    // Refuse a pathologically nested document before parsing it — see
    // `limits`. This is checked here rather than at the HTTP layer so every
    // entry point (network fetch, `--from-file`, library caller) is covered.
    if matches!(media, Media::Html) {
        if let Some(depth) = limits::too_deeply_nested(body) {
            return Ok(too_complex_result(source_url, depth, options.content_type));
        }
    }

    let (title, content, references, metadata, output_type, window, sections, matches) =
        match &media {
            Media::Html => convert_html_body(body, source_url, content_type_header, options)?,
            Media::Json => {
                // Pretty-print so an agent reads clean JSON; fall back to raw.
                let pretty = serde_json::from_str::<serde_json::Value>(body)
                    .ok()
                    .and_then(|v| serde_json::to_string_pretty(&v).ok())
                    .unwrap_or_else(|| body.trim().to_string());
                let (content, window) = budget_plain(&pretty, options)?;
                (
                    String::new(),
                    content,
                    Vec::new(),
                    Metadata::default(),
                    ContentType::Structured,
                    window,
                    Vec::new(),
                    Vec::new(),
                )
            }
            Media::Text => {
                let (content, window) = budget_plain(body.trim(), options)?;
                (
                    String::new(),
                    content,
                    Vec::new(),
                    Metadata::default(),
                    ContentType::Text,
                    window,
                    Vec::new(),
                    Vec::new(),
                )
            }
            Media::Other(ct) => {
                let content = format!(
                    "[non-text content: {ct}, {} bytes — not rendered]",
                    body.len()
                );
                let window = Window::whole(&content);
                (
                    String::new(),
                    content,
                    Vec::new(),
                    Metadata::default(),
                    options.content_type,
                    window,
                    Vec::new(),
                    Vec::new(),
                )
            }
        };

    Ok(FetchResult {
        token_estimate: compress::estimate_tokens(&content),
        total_token_estimate: window.total_token_estimate,
        total_bytes: window.total_bytes,
        offset: window.offset,
        next_offset: window.next_offset,
        truncated: window.next_offset.is_some(),
        outline: sections,
        matches,
        status: classify_content(&media, &content, body),
        title,
        final_url: source_url.to_string(),
        content,
        content_type: output_type,
        media: media.label(),
        references,
        metadata,
        source: source_url.to_string(),
    })
}

/// Refuse an `offset` that cannot start inside the text it indexes.
///
/// An offset of zero is always valid, so an empty page still reaches the
/// "no text" classification. Past the end, the window would be empty and the
/// caller would be told the page renders with JavaScript, which is not why.
fn check_offset(offset: usize, total_bytes: usize) -> Result<(), OptionsError> {
    if offset > 0 && offset >= total_bytes {
        return Err(OptionsError::OffsetPastEnd {
            offset,
            total_bytes,
        });
    }
    Ok(())
}

/// How much of the extracted body one result covers.
///
/// A budgeted fetch returns a slice of a document, and until now said nothing
/// about the document: a page cut at 4000 tokens and a page that *is* 4000
/// tokens were indistinguishable, and there was no way to ask for the rest.
/// This is that accounting, computed where the cut is actually made.
struct Window {
    /// Estimated tokens of the whole extracted body, ignoring budget and offset.
    total_token_estimate: usize,
    /// Size of the whole extracted body in bytes — the space offsets index into.
    total_bytes: usize,
    /// Byte offset the returned content starts at.
    offset: usize,
    /// Byte offset to resume from, or `None` when the body ended here.
    next_offset: Option<usize>,
}

impl Window {
    /// A window covering an entire (short, unbudgeted) document.
    fn whole(text: &str) -> Self {
        Self {
            total_token_estimate: compress::estimate_tokens(text),
            total_bytes: text.len(),
            offset: 0,
            next_offset: None,
        }
    }

    /// The window a budgeted pass produced over `full`, having started at
    /// `offset` and consumed `consumed` bytes from there.
    fn measured(full: &str, offset: usize, consumed: usize) -> Self {
        let start = offset.min(full.len());
        let end = start.saturating_add(consumed).min(full.len());
        Self {
            total_token_estimate: compress::estimate_tokens(full),
            total_bytes: full.len(),
            offset: start,
            // Only report a resume point when there is genuinely more to read.
            // Reporting `end` at the document's end would loop a caller that
            // follows next_offset until it disappears.
            next_offset: (end < full.len()).then_some(end),
        }
    }
}

/// The result returned for a document refused by [`limits::too_deeply_nested`].
fn too_complex_result(source_url: &str, depth: usize, content_type: ContentType) -> FetchResult {
    let content = format!(
        "[document refused: nesting depth {depth} exceeds the limit of {} — \
         parsing it would take minutes]",
        limits::MAX_NESTING_DEPTH
    );
    FetchResult {
        token_estimate: compress::estimate_tokens(&content),
        total_token_estimate: compress::estimate_tokens(&content),
        total_bytes: content.len(),
        offset: 0,
        next_offset: None,
        truncated: false,
        outline: Vec::new(),
        matches: Vec::new(),
        status: ContentStatus::TooComplex,
        title: String::new(),
        final_url: source_url.to_string(),
        content,
        content_type,
        media: "html".to_string(),
        references: Vec::new(),
        metadata: Metadata::default(),
        source: source_url.to_string(),
    }
}

/// The HTML branch of [`convert_body`], kept separate because it is the only
/// one that parses a document — and it now parses exactly once. Title, metadata
/// and the conversion itself all read the same tree; the previous version
/// parsed for the first two and then parsed again inside the converter, which
/// measured at roughly a third of the whole pipeline's cost.
#[allow(clippy::type_complexity)]
fn convert_html_body(
    body: &str,
    source_url: &str,
    content_type_header: Option<&str>,
    options: &FetchOptions,
) -> Result<
    (
        String,
        String,
        Vec<UrlReference>,
        Metadata,
        ContentType,
        Window,
        Vec<outline::Section>,
        Vec<grep::Match>,
    ),
    OptionsError,
> {
    let doc = Html::parse_document(body);
    let title = extract::extract_title(&doc);
    let mut metadata = extract::extract_metadata(&doc);
    metadata.charset = undecodable_charset(content_type_header, &doc);

    let converted = convert::convert_parsed(&doc, source_url, options.content_type);
    // Drop a leading body line that merely repeats the title (common when the
    // title was derived from the page's first <h1>, which also opens the body).
    let (body_text, citations) =
        strip_duplicate_title(&title, converted.content, converted.citations);

    // The outline is a different view of the same document, not a slice of it:
    // it replaces the body with a map of where the body's sections are. Built
    // against the finished text so its offsets are the ones `offset` reads.
    // Searching is a third view of the same document, beside reading it in
    // windows and mapping it by heading: it answers "where does this mention X"
    // on a page whose headings do not say, or that has none. Its offsets are
    // the same ones `offset` reads.
    if let Some(pattern) = &options.grep {
        let whole = Window {
            total_token_estimate: compress::estimate_tokens(&body_text),
            total_bytes: body_text.len(),
            offset: 0,
            next_offset: None,
        };
        let regex = grep::compile(pattern).map_err(|e| OptionsError::InvalidPattern {
            pattern: pattern.clone(),
            reason: e.to_string(),
        })?;
        let sections = outline::outline(&doc, &body_text);
        let matches = grep::grep(&body_text, &regex, &sections);
        let rendered = grep::render(&matches, pattern, options.max_tokens);
        return Ok((
            title,
            rendered,
            Vec::new(),
            metadata,
            options.content_type,
            whole,
            Vec::new(),
            matches,
        ));
    }

    if options.outline {
        let sections = outline::outline(&doc, &body_text);
        let content = outline::render(&sections, options.max_tokens);
        return Ok((
            title,
            content,
            Vec::new(),
            metadata,
            options.content_type,
            Window {
                total_token_estimate: compress::estimate_tokens(&body_text),
                total_bytes: body_text.len(),
                offset: 0,
                // An outline is complete in itself; there is no next window of
                // it to fetch, and reporting one would send a caller paging
                // through a map instead of the document.
                next_offset: None,
            },
            sections,
            Vec::new(),
        ));
    }

    let (content, references, window) = match options.content_type {
        // Reference-style text: the body cites `[N]`, so the budget rule is
        // "truncate the body, then keep the references it still cites".
        ContentType::Text => {
            check_offset(options.offset, body_text.len())?;
            let windowed = compress::window_from(&body_text, options.offset);
            let start = body_text.len() - windowed.len();
            let citations = refs::rebase(&citations, start);
            let fitted = refs::fit_to_budget(
                windowed,
                &citations,
                &converted.references,
                options.max_tokens,
            );
            let references = converted
                .references
                .into_iter()
                .filter(|r| fitted.kept.contains(&r.index))
                .collect();
            let window = Window::measured(&body_text, options.offset, fitted.body_consumed);
            (fitted.content, references, window)
        }
        // Markdown carries its links inline, so the references are the links
        // whose markers survive the window, not the URLs found in its text.
        ContentType::Markdown => {
            check_offset(options.offset, body_text.len())?;
            let windowed = compress::window_from(&body_text, options.offset);
            let start = body_text.len() - windowed.len();
            let (content, consumed) = truncate_window(windowed, options.max_tokens);
            let cited = refs::cited_within(&refs::rebase(&citations, start), consumed);
            let references = converted
                .references
                .into_iter()
                .filter(|r| cited.contains(&r.index))
                .collect();
            let window = Window::measured(&body_text, options.offset, consumed);
            (content, references, window)
        }
        // The content is JSON; truncating its text would produce invalid JSON,
        // so blocks are dropped instead and the document re-serialized. Byte
        // offsets would cut mid-structure, so this format is not windowed.
        ContentType::Structured => {
            let (content, references) = budget_structured(&doc, source_url, options.max_tokens);
            let window = Window {
                total_token_estimate: compress::estimate_tokens(&body_text),
                total_bytes: body_text.len(),
                offset: 0,
                next_offset: None,
            };
            (content, references, window)
        }
    };

    Ok((
        title,
        content,
        references,
        metadata,
        options.content_type,
        window,
        Vec::new(),
        Vec::new(),
    ))
}

/// Window and truncate free-form text (no reference markers to preserve),
/// reporting the slice of the document it covers.
fn budget_plain(text: &str, options: &FetchOptions) -> Result<(String, Window), OptionsError> {
    check_offset(options.offset, text.len())?;
    let windowed = compress::window_from(text, options.offset);
    let (content, consumed) = truncate_window(windowed, options.max_tokens);
    Ok((content, Window::measured(text, options.offset, consumed)))
}

/// Cut an already-windowed text to the token budget. Returns the text and how
/// many of its bytes it consumed.
fn truncate_window(windowed: &str, max_tokens: Option<usize>) -> (String, usize) {
    match max_tokens {
        Some(max) => compress::truncate_to_tokens_at(windowed, max),
        None => (windowed.to_string(), windowed.len()),
    }
}

/// Fit a structured document to the token budget by dropping trailing blocks
/// and re-serializing, so the output is always valid JSON.
///
/// Block count is monotone in serialized size, so a binary search finds the
/// largest prefix that fits in a handful of serializations rather than one per
/// dropped block.
fn budget_structured(
    doc: &Html,
    source_url: &str,
    max_tokens: Option<usize>,
) -> (String, Vec<UrlReference>) {
    use convert::structured::{to_json, StructuredDoc};

    let (parsed, cited_blocks) = convert::structured::structured_with_citations(doc, source_url);

    let render = |n: usize| -> (String, Vec<UrlReference>) {
        let blocks = parsed.blocks[..n].to_vec();
        let references: Vec<UrlReference> = parsed
            .references
            .iter()
            .filter(|r| {
                cited_blocks[..n]
                    .iter()
                    .any(|cited| cited.contains(&r.index))
            })
            .cloned()
            .collect();
        let json = to_json(&StructuredDoc {
            blocks,
            references: references.clone(),
        });
        (json, references)
    };

    let Some(max) = max_tokens else {
        return render(parsed.blocks.len());
    };

    let full = render(parsed.blocks.len());
    if compress::estimate_tokens(&full.0) <= max {
        return full;
    }

    // Largest block count that fits.
    let (mut lo, mut hi) = (0usize, parsed.blocks.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if compress::estimate_tokens(&render(mid).0) <= max {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    render(lo)
}

/// Decide whether an extraction actually produced content, and if not, whether
/// the page looks like it needs a browser.
fn classify_content(media: &Media, content: &str, raw: &str) -> ContentStatus {
    let empty = match media {
        // An empty structured document still serializes to a JSON envelope.
        Media::Html => content.trim().is_empty() || is_empty_structured(content),
        _ => content.trim().is_empty(),
    };
    if !empty {
        return ContentStatus::Ok;
    }
    if matches!(media, Media::Html) && has_scripts(raw) {
        return ContentStatus::NeedsJs;
    }
    ContentStatus::Empty
}

/// A structured render of a page with nothing in it.
fn is_empty_structured(content: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(content)
        .ok()
        .and_then(|v| {
            v.get("blocks")
                .and_then(|b| b.as_array())
                .map(|b| b.is_empty())
        })
        .unwrap_or(false)
}

/// Does the raw document carry scripts? A shell with scripts and no text is a
/// client-rendered page, not an empty one.
///
/// Scans in place rather than lowercasing the whole body: this runs on bodies
/// of up to the 5 MiB cap, and allocating a second copy of one to answer a
/// yes/no question is not worth it.
fn has_scripts(raw: &str) -> bool {
    raw.as_bytes()
        .windows(7)
        .any(|w| w.eq_ignore_ascii_case(b"<script"))
}

/// Report a declared charset no known encoding matches.
///
/// The network path decodes the body before it reaches here (see
/// `webfetch_core::charset`), so this only fires for offline callers passing a
/// header in directly, and only for labels `encoding_rs` does not recognize at
/// all — every encoding in the WHATWG standard decodes exactly.
fn undecodable_charset(header: Option<&str>, doc: &Html) -> Option<String> {
    let declared = header.and_then(charset::from_content_type).or_else(|| {
        let sel = Selector::parse("meta[charset]").ok()?;
        doc.select(&sel)
            .next()
            .and_then(|el| el.value().attr("charset"))
            .map(|c| c.to_string())
    })?;

    match charset::classify(&declared) {
        charset::Charset::Unknown(name) => Some(name),
        _ => None,
    }
}

/// When the title was derived from the page's first heading, the body repeats
/// it as its opening line. Drop that leading line when it normalizes to the
/// same text as `title`. Conservative: only an exact normalized match of the
/// *first* line is removed, so genuine content is never lost. Markers in the
/// removed line go with it, and the rest are moved to the new offsets.
fn strip_duplicate_title(
    title: &str,
    content: String,
    citations: Vec<refs::Citation>,
) -> (String, Vec<refs::Citation>) {
    if title.is_empty() {
        return (content, citations);
    }
    let mut parts = content.splitn(2, '\n');
    let first = parts.next().unwrap_or("");
    if compress::compress_text(first) == compress::compress_text(title) {
        let rest = parts.next().unwrap_or("").trim_start_matches('\n');
        let start = content.len() - rest.len();
        return (rest.to_string(), refs::rebase(&citations, start));
    }
    (content, citations)
}

/// Convert an already-fetched page according to `options`.
///
/// Split out of [`fetch_and_convert`] so a caller holding a page — one served
/// from its own cache, say, which is how a document is paged without refetching
/// it per window — converts it exactly as a live fetch would.
pub fn convert_page(
    page: fetch::FetchedPage,
    options: &FetchOptions,
) -> Result<FetchResult, OptionsError> {
    let mut result = convert_body(
        &page.body,
        &page.final_url,
        page.content_type.as_deref(),
        options,
    )?;
    // `source` is what was asked for, `final_url` is where it came from. They
    // were both set to the post-redirect URL, which discarded the request.
    result.source = options.url.clone();
    result.final_url = page.final_url;
    // The fetch layer knows what it actually decoded with, including the
    // `<meta charset>` fallback, so its verdict wins over re-deriving one here.
    if page.undecodable_charset.is_some() {
        result.metadata.charset = page.undecodable_charset;
    }
    Ok(result)
}

/// Fetch a URL and convert it according to `options`.
#[cfg(feature = "native")]
pub async fn fetch_and_convert(options: FetchOptions) -> anyhow::Result<FetchResult> {
    let page = fetch::fetch_page(&options.url, options.timeout_secs, &options.tls).await?;
    Ok(convert_page(page, &options)?)
}

/// Parse a content-type string ("text" | "markdown" | "structured").
pub fn parse_content_type(s: &str) -> ContentType {
    ContentType::parse(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only labels no encoding matches are reported. Everything in the WHATWG
    /// standard decodes exactly, so flagging it would be noise.
    #[test]
    fn only_unrecognized_charsets_are_reported() {
        let doc = Html::parse_document("<html></html>");
        for header in [
            "text/html; charset=utf-8",
            "text/html; charset=ISO-8859-1",
            "text/html; charset=Shift_JIS",
            "text/html; charset=GBK",
        ] {
            assert_eq!(undecodable_charset(Some(header), &doc), None, "{header}");
        }
        let doc = Html::parse_document(r#"<html><head><meta charset="x-made-up"></head></html>"#);
        assert_eq!(undecodable_charset(None, &doc), Some("x-made-up".into()));
    }

    #[test]
    fn script_shell_is_needs_js_not_empty() {
        let html =
            "<html><body><div id=\"root\"></div><script src=\"/app.js\"></script></body></html>";
        let r = convert_html(html, "https://spa.test/", &FetchOptions::default()).unwrap();
        assert_eq!(r.status, ContentStatus::NeedsJs);
        assert!(r.status.is_failure());
    }

    /// A page whose only literal `[1]` is a footnote (a same-page `#cite_note`
    /// anchor, which earns no reference), followed by filler and then a real
    /// link that is reference #1. The footnote must never pull that link in.
    fn footnote_page(filler_words: usize) -> String {
        let filler = "lorem ipsum dolor sit amet ".repeat(filler_words);
        format!(
            r##"<html><head><title>Ownership</title></head><body><article>
            <p>Values have exactly one owner.<sup class="reference"><a href="#cite_note-1">[1]</a></sup> Values are moved.</p>
            <p>{filler}</p>
            <p>Read the <a href="https://example.com/first">first link</a> and the <a href="https://example.com/second">second link</a>.</p>
            </article></body></html>"##
        )
    }

    #[test]
    fn literal_footnote_text_does_not_pull_in_a_reference() {
        let html = footnote_page(3);
        let r = convert_html(&html, "https://x.test/", &FetchOptions::default()).unwrap();
        assert!(r.content.contains("owner.[1] Values"), "{}", r.content);
        let urls: Vec<_> = r
            .references
            .iter()
            .map(|r| (r.index, r.url.as_str()))
            .collect();
        assert_eq!(
            urls,
            vec![
                (1, "https://example.com/first"),
                (2, "https://example.com/second")
            ]
        );
    }

    #[test]
    fn literal_footnote_text_in_a_budgeted_window_lists_no_reference() {
        // The budget cuts the page before the real links: the only `[1]` in the
        // returned window is the footnote, so the list must be empty.
        let html = footnote_page(300);
        let options = FetchOptions {
            max_tokens: Some(120),
            ..FetchOptions::default()
        };
        let r = convert_html(&html, "https://x.test/", &options).unwrap();
        assert!(r.content.contains("owner.[1]"), "{}", r.content);
        assert!(r.references.is_empty(), "refs: {:?}", r.references);
        assert!(!r.content.contains("References:"), "{}", r.content);
    }

    #[test]
    fn a_window_reports_only_the_references_it_contains() {
        // Start the window at the real links: they are renumbered from the
        // document, so the list must name them by their document indices.
        let html = footnote_page(300);
        let whole = convert_html(&html, "https://x.test/", &FetchOptions::default()).unwrap();
        let start = whole.content.find("Read the").expect("link paragraph");
        let options = FetchOptions {
            offset: start,
            ..FetchOptions::default()
        };
        let r = convert_html(&html, "https://x.test/", &options).unwrap();
        assert!(r.content.contains("first link [1]"), "{}", r.content);
        let urls: Vec<_> = r
            .references
            .iter()
            .map(|r| (r.index, r.url.as_str()))
            .collect();
        assert_eq!(
            urls,
            vec![
                (1, "https://example.com/first"),
                (2, "https://example.com/second")
            ]
        );
    }

    #[test]
    fn markdown_window_ignores_a_url_that_is_only_text_prefix() {
        // Literal text mentions a URL that is a prefix of a real link. The link
        // itself is cut off by the budget, so it must not be listed.
        let filler = "lorem ipsum dolor sit amet ".repeat(300);
        let html = format!(
            r#"<html><body><article><p>See https://example.com/doc/index.html for more.</p>
            <p>{filler}</p><p><a href="https://example.com/doc">the doc</a></p></article></body></html>"#
        );
        let options = FetchOptions {
            content_type: ContentType::Markdown,
            max_tokens: Some(60),
            ..FetchOptions::default()
        };
        let r = convert_html(&html, "https://x.test/", &options).unwrap();
        assert!(r.content.contains("index.html"), "{}", r.content);
        assert!(r.references.is_empty(), "refs: {:?}", r.references);
    }

    #[test]
    fn structured_budget_lists_only_references_in_kept_blocks() {
        let filler = "lorem ipsum dolor sit amet ".repeat(300);
        let html = format!(
            r##"<html><body><article>
            <p>Values have exactly one owner.<sup><a href="#cite_note-1">[1]</a></sup></p>
            <p>{filler} <a href="https://example.com/late">late</a></p>
            </article></body></html>"##
        );
        let options = FetchOptions {
            content_type: ContentType::Structured,
            max_tokens: Some(80),
            ..FetchOptions::default()
        };
        let r = convert_html(&html, "https://x.test/", &options).unwrap();
        assert!(r.content.contains("owner.[1]"), "{}", r.content);
        assert!(!r.content.contains("example.com"), "{}", r.content);
        assert!(r.references.is_empty(), "refs: {:?}", r.references);
    }

    #[test]
    fn a_page_with_text_is_ok() {
        let html = "<html><body><article><p>Real words here.</p></article></body></html>";
        let r = convert_html(html, "https://x.test/", &FetchOptions::default()).unwrap();
        assert_eq!(r.status, ContentStatus::Ok);
    }

    fn options(max_tokens: Option<usize>, offset: usize, grep: Option<&str>) -> FetchOptions {
        FetchOptions {
            max_tokens,
            offset,
            grep: grep.map(str::to_string),
            ..FetchOptions::default()
        }
    }

    /// `max_tokens: 0` used to return one character flagged as truncated, since
    /// the window always makes one character of progress. It is refused now.
    #[test]
    fn a_zero_token_budget_is_refused() {
        let html = "<html><body><article><p>Some words.</p></article></body></html>";
        let err = convert_html(html, "https://x.test/", &options(Some(0), 0, None)).unwrap_err();
        assert_eq!(err, OptionsError::ZeroMaxTokens);
        assert_eq!(
            options(Some(0), 0, None).validate(),
            Err(OptionsError::ZeroMaxTokens)
        );
        assert!(options(Some(1), 0, None).validate().is_ok());
        assert!(options(None, 0, None).validate().is_ok());
    }

    /// An invalid pattern is an error the caller can act on, not text that
    /// reads as a successful result.
    #[test]
    fn an_invalid_grep_pattern_is_an_error_not_content() {
        let html = "<html><body><article><p>Some words.</p></article></body></html>";
        let err =
            convert_html(html, "https://x.test/", &options(None, 0, Some("([a-"))).unwrap_err();
        match err {
            OptionsError::InvalidPattern { pattern, reason } => {
                assert_eq!(pattern, "([a-");
                assert!(!reason.is_empty());
            }
            other => panic!("expected InvalidPattern, got {other:?}"),
        }
        assert!(options(None, 0, Some("([a-")).validate().is_err());
        assert!(options(None, 0, Some("rate limit")).validate().is_ok());
    }

    /// An offset past the end is a request error that names the size of the
    /// content, not a page that "renders its body with JavaScript".
    #[test]
    fn an_offset_past_the_end_names_the_content_size() {
        let html = "<html><body><article><p>Some words here.</p></article></body></html>";
        let total = convert_html(html, "https://x.test/", &FetchOptions::default())
            .unwrap()
            .total_bytes;
        let err = convert_html(html, "https://x.test/", &options(None, 999_999, None)).unwrap_err();
        assert_eq!(
            err,
            OptionsError::OffsetPastEnd {
                offset: 999_999,
                total_bytes: total
            }
        );
        assert_eq!(
            err.to_string(),
            format!("offset 999999 is past the end of the content ({total} bytes)")
        );
        // The last byte is still inside the content.
        let last = convert_html(html, "https://x.test/", &options(None, total - 1, None)).unwrap();
        assert_eq!(last.total_bytes, total);
    }

    /// Offset zero is always valid: an empty page keeps its own status, and a
    /// script shell still reports that it needs JavaScript.
    #[test]
    fn an_empty_page_still_gets_its_own_status_at_offset_zero() {
        let empty = convert_html(
            "<html><body></body></html>",
            "https://x.test/",
            &options(None, 0, None),
        )
        .unwrap();
        assert_eq!(empty.status, ContentStatus::Empty);

        let shell =
            "<html><body><div id=\"root\"></div><script src=\"/app.js\"></script></body></html>";
        let r = convert_html(shell, "https://spa.test/", &options(None, 0, None)).unwrap();
        assert_eq!(r.status, ContentStatus::NeedsJs);

        // Past the end of a page with no text, the offset is what is wrong.
        let err = convert_html(
            "<html><body></body></html>",
            "https://x.test/",
            &options(None, 5, None),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            OptionsError::OffsetPastEnd { total_bytes: 0, .. }
        ));
    }

    /// Structured output is addressed by block, not byte, so an offset there is
    /// ignored rather than checked.
    #[test]
    fn structured_output_ignores_the_offset() {
        let html = "<html><body><article><p>Some words here.</p></article></body></html>";
        let opts = FetchOptions {
            content_type: ContentType::Structured,
            offset: 999_999,
            ..FetchOptions::default()
        };
        assert!(convert_html(html, "https://x.test/", &opts).is_ok());
    }
}
