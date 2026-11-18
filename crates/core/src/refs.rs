//! Shared reference-style URL preservation.
//!
//! Both the fetch path and the search path cite URLs with inline `[N]` markers
//! and collect the full URLs into a trailing block. This module owns the one
//! canonical rendering of that block, and the budgeting rule that keeps the
//! block and the body it belongs to inside a token cap together.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::compress::{estimate_tokens, truncate_to_tokens_at};

/// Anything that can be listed in a reference block: an index and a URL.
pub trait Referable {
    fn index(&self) -> usize;
    fn url(&self) -> &str;
}

impl<T: Referable> Referable for &T {
    fn index(&self) -> usize {
        (*self).index()
    }
    fn url(&self) -> &str {
        (*self).url()
    }
}

/// A slim reference entry (index → URL) for an output's reference block.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Reference {
    pub index: usize,
    pub url: String,
}

impl Referable for Reference {
    fn index(&self) -> usize {
        self.index
    }
    fn url(&self) -> &str {
        &self.url
    }
}

/// Render references into the canonical block:
///
/// ```text
/// References:
/// [1] https://example.com/a
/// [2] https://example.com/b
/// ```
///
/// Returns an empty string when there are no references.
pub fn render_block<T: Referable>(references: &[T]) -> String {
    if references.is_empty() {
        return String::new();
    }
    let mut s = String::from("References:\n");
    for r in references {
        s.push_str(&format!("[{}] {}\n", r.index(), r.url()));
    }
    s.truncate(s.trim_end().len());
    s
}

/// The smallest body budget we will ever leave, so a page dominated by links
/// still shows *some* body rather than collapsing to a bare reference list.
const MIN_BODY_TOKENS: usize = 64;

/// How many times we re-shrink the body budget trying to fit body + block.
/// Each pass drops uncited references, which shrinks the block, which frees
/// budget — the sequence is monotone and converges in two or three passes.
const FIT_PASSES: usize = 6;

/// One reference marker the converter emitted: the byte range of its visible
/// text in a body, and the reference it cites.
///
/// Markers are recorded where they are emitted, not recovered by scanning the
/// text. Page text can contain `[74]` of its own (a footnote whose anchor
/// points at a same-page target earns no reference), and a scan cannot tell
/// that from a marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Citation {
    pub start: usize,
    pub end: usize,
    pub index: usize,
}

/// The reference indices whose markers lie entirely within the first `len`
/// bytes of the body they were recorded against.
pub fn cited_within(citations: &[Citation], len: usize) -> BTreeSet<usize> {
    citations
        .iter()
        .filter(|c| c.end <= len)
        .map(|c| c.index)
        .collect()
}

/// The citations inside `body[start..]`, moved so they index that slice.
///
/// A marker that begins before `start` is not in the slice, even if its tail
/// is, so it is dropped rather than clipped.
pub fn rebase(citations: &[Citation], start: usize) -> Vec<Citation> {
    citations
        .iter()
        .filter(|c| c.start >= start)
        .map(|c| Citation {
            start: c.start - start,
            end: c.end - start,
            index: c.index,
        })
        .collect()
}

/// Join a body and a rendered reference block into final output.
fn assemble(body: &str, block: &str) -> String {
    if block.is_empty() {
        body.to_string()
    } else {
        format!("{body}\n\n{block}")
    }
}

/// What [`fit_to_budget`] produced: the assembled output, the references it
/// kept, and how much of the body it got through.
pub struct Fitted {
    /// Body plus reference block, inside the cap.
    pub content: String,
    /// Indices of the references the surviving body still cites.
    pub kept: Vec<usize>,
    /// Bytes of `body` consumed. A caller paging a long document resumes here;
    /// equal to `body.len()` when the whole body fit.
    pub body_consumed: usize,
}

/// Fit `body` plus its reference block inside `max_tokens`.
///
/// `citations` are the markers the converter emitted into `body`, with offsets
/// relative to `body`. A reference is kept only if one of those markers survives
/// the truncation.
///
/// Returns the assembled content, the reference indices it kept, and how far
/// into the body it got.
///
/// The old rule reserved room for the *whole* reference block and then appended
/// it regardless of size, so a link-dense page blew straight through the cap
/// (a 120-link page answered `--max-tokens 200` with ~3300 tokens). The rule
/// here is the other way round: truncate the body first, then keep only the
/// references the surviving text still cites. Dropping a reference nobody cites
/// costs nothing and is usually enough on its own; if the block still does not
/// fit, references are dropped from the tail so the cap holds.
///
/// With `max_tokens == None` nothing is truncated and every reference is kept.
pub fn fit_to_budget<T: Referable>(
    body: &str,
    citations: &[Citation],
    references: &[T],
    max_tokens: Option<usize>,
) -> Fitted {
    let all = || references.iter().map(Referable::index).collect::<Vec<_>>();

    let Some(max_tokens) = max_tokens else {
        return Fitted {
            content: assemble(body, &render_block(references)),
            kept: all(),
            body_consumed: body.len(),
        };
    };

    let full_len = body.len();
    let mut budget = max_tokens;
    for pass in 0..FIT_PASSES {
        let (cut, consumed) = truncate_to_tokens_at(body, budget);
        let cited = cited_within(citations, consumed);
        let kept: Vec<&T> = references
            .iter()
            .filter(|r| cited.contains(&r.index()))
            .collect();
        let block = render_block(&kept);
        let content = assemble(&cut, &block);
        let total = estimate_tokens(&content);

        if total <= max_tokens {
            return Fitted {
                content,
                kept: kept.iter().map(|r| r.index()).collect(),
                body_consumed: consumed.min(full_len),
            };
        }
        if pass + 1 == FIT_PASSES || budget <= MIN_BODY_TOKENS {
            // The block alone is over budget (very long URLs, very small cap).
            // Drop references from the tail until the whole thing fits.
            return drop_until_fits(&cut, &kept, max_tokens, consumed.min(full_len));
        }
        // Scale the body budget by how far over we landed, rather than
        // subtracting the overshoot. Subtracting punishes the body for the
        // reference block's size — one long block drove the budget straight to
        // the floor and answered a 1000-token cap with under 300 tokens of
        // output. Scaling converges on the cap from below in two or three
        // passes instead.
        let scaled = (budget as u128 * max_tokens as u128 / total as u128) as usize;
        let next = scaled.max(MIN_BODY_TOKENS);
        if next >= budget {
            // Not converging (already at the floor): stop shrinking the body
            // and take references off the tail instead.
            return drop_until_fits(&cut, &kept, max_tokens, consumed.min(full_len));
        }
        budget = next;
    }
    unreachable!("the loop returns on its final pass")
}

/// Last resort: shrink the reference block itself, tail first. Markers for
/// dropped references stop resolving, which is a worse output than a complete
/// block — but a silently ignored token cap is worse still.
fn drop_until_fits<T: Referable>(
    body: &str,
    kept: &[&T],
    max_tokens: usize,
    body_consumed: usize,
) -> Fitted {
    let mut kept = kept.to_vec();
    while !kept.is_empty() {
        let content = assemble(body, &render_block(&kept));
        if estimate_tokens(&content) <= max_tokens {
            return Fitted {
                content,
                kept: kept.iter().map(|r| r.index()).collect(),
                body_consumed,
            };
        }
        kept.pop();
    }
    Fitted {
        content: body.to_string(),
        kept: Vec::new(),
        body_consumed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(n: usize) -> Vec<Reference> {
        (1..=n)
            .map(|i| Reference {
                index: i,
                url: format!("https://example.com/very/long/path/segment/{i}?query=value#frag"),
            })
            .collect()
    }

    /// A body of `n` items, each ending in a `[i]` marker, with the markers'
    /// offsets recorded as they are written.
    fn body_citing(n: usize) -> (String, Vec<Citation>) {
        let mut body = String::new();
        let mut citations = Vec::new();
        for i in 1..=n {
            if i > 1 {
                body.push('\n');
            }
            body.push_str(&format!("Item {i} with filler prose describing the thing "));
            let start = body.len();
            body.push_str(&format!("[{i}]"));
            citations.push(Citation {
                start,
                end: body.len(),
                index: i,
            });
            body.push('.');
        }
        (body, citations)
    }

    #[test]
    fn literal_bracketed_numbers_are_not_citations() {
        // A footnote's `[1]` is page text; only recorded markers may cite.
        let body = "Values have one owner.[1] Values are moved.";
        let refs = refs(1);
        let Fitted { content, kept, .. } = fit_to_budget(body, &[], &refs, Some(1000));
        assert!(kept.is_empty(), "kept {kept:?}");
        assert!(!content.contains("References:"), "{content}");
    }

    #[test]
    fn cited_within_counts_only_markers_fully_inside() {
        let citations = [
            Citation {
                start: 0,
                end: 3,
                index: 1,
            },
            Citation {
                start: 10,
                end: 13,
                index: 2,
            },
        ];
        assert_eq!(
            cited_within(&citations, 3).into_iter().collect::<Vec<_>>(),
            vec![1]
        );
        assert_eq!(
            cited_within(&citations, 12).into_iter().collect::<Vec<_>>(),
            vec![1]
        );
        assert_eq!(
            cited_within(&citations, 13).into_iter().collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn rebase_drops_markers_before_the_start_and_shifts_the_rest() {
        let citations = [
            Citation {
                start: 2,
                end: 5,
                index: 1,
            },
            Citation {
                start: 8,
                end: 11,
                index: 2,
            },
        ];
        // A marker straddling the start is dropped, not clipped.
        assert_eq!(
            rebase(&citations, 3),
            vec![Citation {
                start: 5,
                end: 8,
                index: 2
            }]
        );
        assert_eq!(rebase(&citations, 0), citations.to_vec());
    }

    #[test]
    fn no_budget_keeps_everything() {
        let (body, citations) = body_citing(3);
        let Fitted { content, kept, .. } = fit_to_budget(&body, &citations, &refs(3), None);
        assert_eq!(kept, vec![1, 2, 3]);
        assert!(content.contains("References:"));
        assert!(content.contains("[3] https://example.com"));
    }

    /// The regression this function exists for: a link-dense page must not
    /// answer a small budget with a full reference block.
    #[test]
    fn link_dense_page_respects_the_cap() {
        let refs = refs(120);
        let (body, citations) = body_citing(120);
        let Fitted { content, kept, .. } = fit_to_budget(&body, &citations, &refs, Some(200));
        assert!(
            estimate_tokens(&content) <= 200,
            "estimate {} content: {content}",
            estimate_tokens(&content)
        );
        assert!(kept.len() < 120, "kept {} of 120", kept.len());
    }

    #[test]
    fn every_kept_reference_is_still_cited() {
        let refs = refs(120);
        let (body, citations) = body_citing(120);
        let Fitted { content, kept, .. } = fit_to_budget(&body, &citations, &refs, Some(300));
        let text = content.split("References:").next().unwrap();
        let cited = cited_within(&citations, text.len());
        for index in &kept {
            assert!(cited.contains(index), "kept [{index}] is not cited");
        }
    }

    /// Fitting the cap is necessary but not sufficient: answering a 1000-token
    /// budget with 280 tokens wastes most of the caller's allowance.
    #[test]
    fn a_generous_budget_is_actually_used() {
        let refs = refs(120);
        let (body, citations) = body_citing(120);
        let Fitted { content, .. } = fit_to_budget(&body, &citations, &refs, Some(1000));
        let used = estimate_tokens(&content);
        assert!(used <= 1000, "over budget: {used}");
        assert!(used >= 800, "left {} of 1000 tokens unused", 1000 - used);
    }

    #[test]
    fn tiny_budget_still_fits() {
        let refs = refs(40);
        let (body, citations) = body_citing(40);
        let Fitted { content, .. } = fit_to_budget(&body, &citations, &refs, Some(80));
        assert!(
            estimate_tokens(&content) <= 80,
            "estimate {}",
            estimate_tokens(&content)
        );
    }

    #[test]
    fn generous_budget_is_a_noop() {
        let refs = refs(3);
        let (body, citations) = body_citing(3);
        let Fitted { content, kept, .. } = fit_to_budget(&body, &citations, &refs, Some(100_000));
        assert_eq!(kept, vec![1, 2, 3]);
        assert_eq!(content, assemble(&body, &render_block(&refs)));
    }

    #[test]
    fn body_without_references_is_plain_truncation() {
        let empty: [Reference; 0] = [];
        let Fitted { content, kept, .. } =
            fit_to_budget(&"word ".repeat(500), &[], &empty, Some(50));
        assert!(kept.is_empty());
        assert!(content.contains("…[truncated]"));
        assert!(estimate_tokens(&content) <= 50);
    }
}
