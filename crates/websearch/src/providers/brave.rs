//! Brave Search API.
//!
//! An independent index behind a real API contract, which is the point: unlike
//! scraped HTML it does not silently change shape or serve a challenge page.
//! The key travels in `X-Subscription-Token`, never in the URL.

use std::time::Duration;

use url::Url;

use crate::types::{SearchOptions, SearchResult, SearchStatus};
#[cfg(feature = "native")]
use webfetch_core::transport::ReqwestTransport;
use webfetch_core::transport::{HttpRequest, Method, Transport};

const ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";

/// Search over the native transport.
#[cfg(feature = "native")]
pub async fn search(
    options: &SearchOptions,
    max: usize,
    api_key: &str,
) -> anyhow::Result<(Vec<SearchResult>, SearchStatus)> {
    let transport = ReqwestTransport::new(&options.tls)?;
    search_with(&transport, options, max, api_key).await
}

/// Search through a caller-supplied transport.
pub async fn search_with(
    transport: &impl Transport,
    options: &SearchOptions,
    max: usize,
    api_key: &str,
) -> anyhow::Result<(Vec<SearchResult>, SearchStatus)> {
    let mut query = vec![
        ("q", options.query.clone()),
        ("count", max.min(20).to_string()),
    ];
    if let Some(safe) = options.safe_search {
        query.push((
            "safesearch",
            if safe { "strict" } else { "off" }.to_string(),
        ));
    }
    let url = Url::parse_with_params(ENDPOINT, query)?;

    let request = HttpRequest {
        method: Method::Get,
        url: url.to_string(),
        headers: vec![
            ("Accept".to_string(), "application/json".to_string()),
            ("X-Subscription-Token".to_string(), api_key.to_string()),
        ],
        body: None,
        timeout: Some(Duration::from_secs(options.timeout_secs)),
    };
    let resp = super::send_following(transport, request, super::MAX_REDIRECTS).await?;
    let body = String::from_utf8_lossy(&resp.body);
    if !(200..300).contains(&resp.status) {
        // The body may carry Brave's own explanation (quota, bad key); the key
        // itself is a header, so nothing here can echo it back.
        anyhow::bail!(
            "brave search failed ({}): {}",
            webfetch_core::http::status_text(resp.status),
            body.trim()
        );
    }

    Ok(parse(&body, max))
}

/// Parse a Brave response body: `{"web": {"results": [{title, url, description}]}}`.
pub(crate) fn parse(body: &str, max: usize) -> (Vec<SearchResult>, SearchStatus) {
    let value: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        // A success status with an unparseable body is a failure, not an empty
        // result — the same distinction the DuckDuckGo path makes.
        Err(_) => return (Vec::new(), SearchStatus::Blocked),
    };
    let items = value
        .get("web")
        .and_then(|w| w.get("results"))
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();

    let results = super::results_from_json(&items, "title", "url", &["description"], max);
    let status = if results.is_empty() {
        SearchStatus::Empty
    } else {
        SearchStatus::Ok
    };
    (results, status)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "type": "search",
      "web": { "results": [
        { "title": "Rust async book", "url": "https://rust-lang.github.io/async-book/",
          "description": "Asynchronous programming in <strong>Rust</strong>." },
        { "title": "Tokio", "url": "https://tokio.rs", "description": "An async runtime." }
      ]}
    }"#;

    #[test]
    fn parses_results_and_numbers_references() {
        let (results, status) = parse(SAMPLE, 10);
        assert_eq!(status, SearchStatus::Ok);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].url, "https://rust-lang.github.io/async-book/");
        assert_eq!(results[0].ref_index, 1);
        assert_eq!(results[1].ref_index, 2);
        assert_eq!(results[1].snippet, "An async runtime.");
    }

    #[test]
    fn respects_max_results() {
        let (results, _) = parse(SAMPLE, 1);
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn no_hits_is_empty_not_blocked() {
        let (results, status) = parse(r#"{"web":{"results":[]}}"#, 10);
        assert!(results.is_empty());
        assert_eq!(status, SearchStatus::Empty);
    }

    #[test]
    fn unparseable_body_is_blocked() {
        let (_, status) = parse("<html>gateway error</html>", 10);
        assert_eq!(status, SearchStatus::Blocked);
    }

    #[test]
    fn missing_web_key_degrades_to_empty() {
        let (results, status) = parse(r#"{"query":{"original":"x"}}"#, 10);
        assert!(results.is_empty());
        assert_eq!(status, SearchStatus::Empty);
    }
}
