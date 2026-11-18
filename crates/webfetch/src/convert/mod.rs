//! Output dispatcher: routes an HTML document to the requested format.

pub mod markdown;
pub mod structured;
pub mod text;

use scraper::Html;

use crate::compress::compress_block;
use crate::refs::Citation;
use crate::types::{ContentType, UrlReference};

/// Elements whose contents never belong in extracted output (scripts,
/// styling, embedded documents). Shared by every walker so the formats
/// agree on what to drop.
pub(crate) fn is_skippable(name: &str) -> bool {
    matches!(
        name,
        "script" | "style" | "noscript" | "svg" | "head" | "template" | "iframe"
    )
}

/// Delimiters around a reference marker while a walker is building its text.
///
/// A marker is written as `OPEN index OPEN visible CLOSE`, and
/// [`resolve_citations`] strips the delimiters out again and records where the
/// visible text landed. Page text never reaches the output with these control
/// characters in it (see [`push_literal`]), so a literal `[74]` in a page is
/// never mistaken for a marker.
const CITE_OPEN: char = '\u{0}';
const CITE_CLOSE: char = '\u{1}';

fn is_delimiter(c: char) -> bool {
    c == CITE_OPEN || c == CITE_CLOSE
}

/// Append page text to a walker's working string, dropping any delimiter
/// characters it carries so they cannot be read as markers.
pub(crate) fn push_literal(out: &mut String, text: &str) {
    if text.contains(is_delimiter) {
        out.extend(text.chars().filter(|c| !is_delimiter(*c)));
    } else {
        out.push_str(text);
    }
}

/// Append a reference marker whose visible text is `visible` (for example
/// `[3]`, or `[anchor](url)` in markdown) and that cites reference `index`.
pub(crate) fn push_citation(out: &mut String, index: usize, visible: &str) {
    out.push(CITE_OPEN);
    out.push_str(&index.to_string());
    out.push(CITE_OPEN);
    out.push_str(visible);
    out.push(CITE_CLOSE);
}

/// Turn a walker's working text into the output text plus the markers in it.
///
/// Delimiters are removed; each marker's visible text is kept in place and its
/// byte range in the returned text is recorded. Malformed delimiters (which the
/// walkers never emit) are dropped rather than trusted.
pub(crate) fn resolve_citations(working: &str) -> (String, Vec<Citation>) {
    enum State {
        Text,
        Index { digits: String },
        Visible { start: usize, index: usize },
    }

    let mut text = String::with_capacity(working.len());
    let mut citations = Vec::new();
    let mut state = State::Text;
    for c in working.chars() {
        state = match state {
            State::Text => match c {
                CITE_OPEN => State::Index {
                    digits: String::new(),
                },
                CITE_CLOSE => State::Text,
                _ => {
                    text.push(c);
                    State::Text
                }
            },
            State::Index { mut digits } => match c {
                CITE_OPEN => match digits.parse::<usize>() {
                    Ok(index) => State::Visible {
                        start: text.len(),
                        index,
                    },
                    Err(_) => State::Text,
                },
                _ if c.is_ascii_digit() => {
                    digits.push(c);
                    State::Index { digits }
                }
                _ => State::Text,
            },
            State::Visible { start, index } => match c {
                CITE_CLOSE => {
                    citations.push(Citation {
                        start,
                        end: text.len(),
                        index,
                    });
                    State::Text
                }
                _ => {
                    text.push(c);
                    State::Visible { start, index }
                }
            },
        };
    }
    (text, citations)
}

/// A converted document: the rendered `content` plus the references it cites.
///
/// `content` never carries the trailing `References:` block — assembling that
/// is [`crate::refs::fit_to_budget`]'s job, because whether a reference survives
/// depends on whether the (possibly truncated) body still cites it. `citations`
/// are the markers in `content`, recorded by the walker that emitted them.
pub struct Converted {
    pub content: String,
    pub references: Vec<UrlReference>,
    pub citations: Vec<Citation>,
}

/// Convert a parsed document to the requested content type.
pub fn convert_parsed(doc: &Html, base_url: &str, content_type: ContentType) -> Converted {
    match content_type {
        ContentType::Text => {
            let (body, references) = text::text_working(doc, base_url);
            let (content, citations) = resolve_citations(&compress_block(&body));
            Converted {
                content,
                references,
                citations,
            }
        }
        ContentType::Markdown => {
            let (md, references) = markdown::markdown_working(doc, base_url);
            let (content, citations) = resolve_citations(&compress_block(&md));
            Converted {
                content,
                references,
                citations,
            }
        }
        ContentType::Structured => {
            let parsed = structured::structured(doc, base_url);
            Converted {
                content: structured::to_json(&parsed),
                references: parsed.references,
                citations: Vec::new(),
            }
        }
    }
}

/// [`convert_parsed`] for callers holding raw HTML.
///
/// Prefer the parsed form: the full pipeline used to parse the same document
/// twice — once for the title and metadata, once here — which was roughly a
/// third of its total cost.
pub fn convert(html: &str, base_url: &str, content_type: ContentType) -> Converted {
    convert_parsed(&Html::parse_document(html), base_url, content_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recorded_marker_is_resolved_to_its_span() {
        let mut working = String::from("see ");
        push_citation(&mut working, 3, "[3]");
        working.push_str(" next");
        let (text, citations) = resolve_citations(&working);
        assert_eq!(text, "see [3] next");
        assert_eq!(
            citations,
            vec![Citation {
                start: 4,
                end: 7,
                index: 3
            }]
        );
    }

    /// Page text cannot introduce a marker: its delimiter characters are
    /// dropped on the way in, so a literal `[74]` stays text.
    #[test]
    fn literal_text_cannot_forge_a_marker() {
        let mut working = String::new();
        push_literal(&mut working, "owner.[74]\u{0}1\u{0}[1]\u{1}");
        let (text, citations) = resolve_citations(&working);
        assert_eq!(text, "owner.[74]1[1]");
        assert!(citations.is_empty(), "{citations:?}");
    }
}
