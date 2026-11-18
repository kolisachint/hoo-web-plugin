//! Markdown conversion. Unlike the text path, markdown keeps links inline as
//! `[text](url)` for maximum fidelity — the right trade-off when the consumer
//! wants a faithful, re-renderable document rather than minimal tokens.
//!
//! Links are still collected into a reference list, so a `--json` caller gets
//! the same recoverable URLs the text path gives it; and the same schemes the
//! text path refuses (`javascript:`, `mailto:`, in-page anchors) are left as
//! plain text rather than emitted as live markdown links.

use ego_tree::NodeRef;
use scraper::node::Node;
use scraper::Html;

use super::text::RefCollector;
use super::{plain, push_citation, push_literal, resolve_citations};
use crate::extract;
use crate::types::UrlReference;

fn walk(node: NodeRef<Node>, out: &mut String, refs: &mut RefCollector) {
    match node.value() {
        Node::Text(t) => push_literal(out, &t[..]),
        Node::Element(el) => {
            let name = el.name();
            if super::is_skippable(name) {
                return;
            }

            let prefix = match name {
                "h1" => Some("\n# "),
                "h2" => Some("\n## "),
                "h3" => Some("\n### "),
                "h4" => Some("\n#### "),
                "h5" => Some("\n##### "),
                "h6" => Some("\n###### "),
                "li" => Some("\n- "),
                "blockquote" => Some("\n> "),
                _ => None,
            };

            if name == "br" {
                out.push('\n');
                return;
            }

            if name == "a" {
                if refs.anchor_depth > 0 {
                    // Nested in a link: its text reads as part of the outer one.
                    for child in node.children() {
                        walk(child, out, refs);
                    }
                    return;
                }
                // Only an anchor that earns a marker suppresses the links inside
                // it. An `<a name>` with no usable href is not a link.
                let resolved = el.attr("href").and_then(|href| refs.resolve(href));
                let linked = resolved.is_some();
                if linked {
                    refs.anchor_depth += 1;
                }
                let mut inner = String::new();
                for child in node.children() {
                    walk(child, &mut inner, refs);
                }
                if linked {
                    refs.anchor_depth -= 1;
                }
                let inner = plain(&inner).trim().to_string();
                match resolved {
                    Some(url) => {
                        let idx = refs.index_for(url.clone(), &inner);
                        push_citation(out, idx, &format!("[{inner}]({url})"));
                    }
                    None => out.push_str(&inner),
                }
                return;
            }

            if name == "code" {
                let mut inner = String::new();
                for child in node.children() {
                    walk(child, &mut inner, refs);
                }
                out.push_str(&format!("`{}`", inner.trim()));
                return;
            }

            if let Some(p) = prefix {
                out.push_str(p);
            }
            for child in node.children() {
                walk(child, out, refs);
            }
            if matches!(
                name,
                "p" | "div" | "section" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
            ) {
                out.push('\n');
            }
        }
        _ => {}
    }
}

/// The markdown walk's working string, still carrying its link delimiters, and
/// the references it collected. See [`super::text::text_working`].
pub(crate) fn markdown_working(doc: &Html, base_url: &str) -> (String, Vec<UrlReference>) {
    let root = match extract::content_root(doc) {
        Some(el) => el,
        None => return (String::new(), Vec::new()),
    };
    let mut refs = RefCollector::new(base_url);
    let mut out = String::new();
    for child in root.children() {
        walk(child, &mut out, &mut refs);
    }
    (out, refs.references)
}

/// Convert a parsed document to markdown, also returning the links it contains.
pub fn markdown_with_refs(doc: &Html, base_url: &str) -> (String, Vec<UrlReference>) {
    let (working, references) = markdown_working(doc, base_url);
    (resolve_citations(&working).0, references)
}

/// [`markdown_with_refs`] for callers holding raw HTML.
pub fn html_to_markdown(html: &str, base_url: &str) -> String {
    markdown_with_refs(&Html::parse_document(html), base_url).0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `<a name>` has no destination, so it does not suppress the link
    /// inside it: the table's link keeps its inline link and reference.
    #[test]
    fn a_link_inside_an_anchor_without_href_keeps_its_link() {
        let html = r#"<article><a name="x"><table><tr><td><a href="https://x.test/b">inner</a></td></tr></table></a></article>"#;
        let (md, refs) = markdown_with_refs(&Html::parse_document(html), "https://x.test/");
        assert_eq!(refs.len(), 1, "refs: {refs:?}");
        assert_eq!(refs[0].url, "https://x.test/b");
        assert!(md.contains("[inner](https://x.test/b)"), "md: {md}");
    }

    #[test]
    fn unfetchable_schemes_stay_plain_text() {
        let html = r#"<article><p><a href="javascript:alert(1)">click</a>
            and <a href="/ok">ok</a></p></article>"#;
        let (md, refs) = markdown_with_refs(&Html::parse_document(html), "https://x.test/");
        assert!(!md.contains("javascript:"), "md: {md}");
        assert!(!md.contains("[click]("), "js link must not stay live: {md}");
        assert!(md.contains("click"), "anchor text is still kept: {md}");
        assert!(md.contains("[ok](https://x.test/ok)"), "md: {md}");
        assert_eq!(refs.len(), 1);
    }
}
