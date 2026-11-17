//! A self-hosted SearXNG instance's JSON API.
//!
//! The option for networks where the public search APIs are unreachable, or
//! where queries must not leave the estate. `base_url` points at the instance
//! root; the optional key travels as a bearer header.

use std::time::Duration;

use url::Url;

use crate::types::{SearchOptions, SearchResult, SearchStatus};
#[cfg(feature = "native")]
use webfetch_core::transport::ReqwestTransport;
use webfetch_core::transport::{HttpRequest, Method, Transport};

/// Search over the native transport.
#[cfg(feature = "native")]
pub async fn search(
    options: &SearchOptions,
    max: usize,
    base_url: &str,
    api_key: Option<&str>,
) -> anyhow::Result<(Vec<SearchResult>, SearchStatus)> {
    let transport = ReqwestTransport::new(&options.tls)?;
    search_with(&transport, options, max, base_url, api_key).await
}

/// Search through a caller-supplied transport.
pub async fn search_with(
    transport: &impl Transport,
    options: &SearchOptions,
    max: usize,
    base_url: &str,
    api_key: Option<&str>,
) -> anyhow::Result<(Vec<SearchResult>, SearchStatus)> {
    let endpoint = format!("{}/search", base_url.trim_end_matches('/'));
    let mut query = vec![("q", options.query.clone()), ("format", "json".to_string())];
    if let Some(safe) = options.safe_search {
        query.push(("safesearch", if safe { "2" } else { "0" }.to_string()));
    }
    let url = Url::parse_with_params(&endpoint, query)?;

    let mut headers = vec![("Accept".to_string(), "application/json".to_string())];
    if let Some(key) = api_key {
        headers.push(("Authorization".to_string(), format!("Bearer {key}")));
    }
    let request = HttpRequest {
        method: Method::Get,
        url: url.to_string(),
        headers,
        body: None,
        timeout: Some(Duration::from_secs(options.timeout_secs)),
    };
    let resp = super::send_following(transport, request, super::MAX_REDIRECTS).await?;
    let body = String::from_utf8_lossy(&resp.body);
    if !(200..300).contains(&resp.status) {
        anyhow::bail!("searxng search failed ({}): {}", resp.status, body.trim());
    }

    Ok(parse(&body, max))
}

/// Parse a SearXNG response body: `{"results": [{title, url, content}]}`.
pub(crate) fn parse(body: &str, max: usize) -> (Vec<SearchResult>, SearchStatus) {
    let value: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return (Vec::new(), SearchStatus::Blocked),
    };
    let items = value
        .get("results")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();

    let results = super::results_from_json(&items, "title", "url", &["content"], max);
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

    #[test]
    fn parses_results() {
        let body = r#"{"query":"x","results":[
            {"url":"https://a.test/1","title":"Alpha","content":"About alpha."},
            {"url":"https://b.test/2","title":"Bravo","content":"About bravo."}]}"#;
        let (results, status) = parse(body, 10);
        assert_eq!(status, SearchStatus::Ok);
        assert_eq!(results.len(), 2);
        assert_eq!(results[1].title, "Bravo");
    }

    #[test]
    fn html_error_page_is_blocked() {
        assert_eq!(parse("<html>502</html>", 10).1, SearchStatus::Blocked);
    }
}
