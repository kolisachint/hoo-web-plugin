//! DuckDuckGo Lite: the keyless default. Scraped HTML, so it is the one
//! provider that can be blocked or reshaped without warning — see
//! [`crate::extract::classify_page`], which turns that into a reported failure
//! instead of a silent empty result.

use std::time::Duration;

use webfetch_core::http::{status_text, transient_status};
#[cfg(feature = "native")]
use webfetch_core::transport::ReqwestTransport;
use webfetch_core::transport::{HttpRequest, Method, Transport};

use crate::extract;
use crate::types::{SearchOptions, SearchResult, SearchStatus};

const MAX_ATTEMPTS: u32 = 3;

/// DDG follows at most one redirect; it uses one to hand off to its challenge
/// flow, and an unbounded chain is not something a search client should walk.
const MAX_REDIRECTS: usize = 1;

/// DDG answers a throttled client with `202 Accepted` and a challenge body
/// rather than a 4xx, so the status is worth naming explicitly.
const CHALLENGE_STATUS: u16 = 202;

/// Search over the native transport.
#[cfg(feature = "native")]
pub async fn search(
    options: &SearchOptions,
    max: usize,
) -> anyhow::Result<(Vec<SearchResult>, SearchStatus)> {
    let transport = ReqwestTransport::new(&options.tls)?;
    search_with(&transport, options, max).await
}

/// Search through a caller-supplied transport.
pub async fn search_with(
    transport: &impl Transport,
    options: &SearchOptions,
    max: usize,
) -> anyhow::Result<(Vec<SearchResult>, SearchStatus)> {
    let html = fetch_lite_with(transport, &options.query, options).await?;
    let results = extract::parse_ddg_lite(&html, max);
    let status = extract::classify_page(&html, results.len());
    Ok((results, status))
}

/// Fetch the raw DuckDuckGo Lite results page for a query over the native
/// transport.
#[cfg(feature = "native")]
pub async fn fetch_lite(query: &str, options: &SearchOptions) -> anyhow::Result<String> {
    let transport = ReqwestTransport::new(&options.tls)?;
    fetch_lite_with(&transport, query, options).await
}

/// Fetch the raw DuckDuckGo Lite results page for a query, retrying transient
/// failures (connection/timeout, 5xx, 429) with exponential backoff.
pub async fn fetch_lite_with(
    transport: &impl Transport,
    query: &str,
    options: &SearchOptions,
) -> anyhow::Result<String> {
    let mut url = format!(
        "https://lite.duckduckgo.com/lite/?q={}",
        urlencoding::encode(query)
    );
    // DDG safe-search toggle: kp=1 strict, kp=-1 off.
    if let Some(safe) = options.safe_search {
        url.push_str(if safe { "&kp=1" } else { "&kp=-1" });
    }

    let request = HttpRequest {
        method: Method::Get,
        url,
        headers: vec![
            (
                "Accept".to_string(),
                "text/html,application/xhtml+xml".to_string(),
            ),
            ("Accept-Language".to_string(), "en-US,en;q=0.9".to_string()),
        ],
        body: None,
        timeout: Some(Duration::from_secs(options.timeout_secs)),
    };

    let mut delay = Duration::from_millis(200);
    for attempt_no in 1..=MAX_ATTEMPTS {
        match attempt(transport, &request).await {
            Ok(body) => return Ok(body),
            Err((err, transient)) => {
                if attempt_no == MAX_ATTEMPTS || !transient {
                    return Err(err);
                }
                transport.sleep(delay).await;
                delay *= 2;
            }
        }
    }
    unreachable!("loop returns on the final attempt")
}

/// One request attempt; the bool reports whether a failure is worth retrying.
async fn attempt(
    transport: &impl Transport,
    request: &HttpRequest,
) -> Result<String, (anyhow::Error, bool)> {
    let resp = match super::send_following(transport, request.clone(), MAX_REDIRECTS).await {
        Ok(r) => r,
        Err(e) => {
            let transient = e.is_transient();
            return Err((e.into(), transient));
        }
    };
    let status = resp.status;
    if status == CHALLENGE_STATUS {
        return Err((
            anyhow::anyhow!(
                "DuckDuckGo returned {} (rate limited or challenged)",
                status_text(status)
            ),
            true,
        ));
    }
    if status >= 400 {
        let kind = if status >= 500 {
            "server error"
        } else {
            "client error"
        };
        return Err((
            anyhow::anyhow!(
                "HTTP status {kind} ({}) for url ({})",
                status_text(status),
                request.url
            ),
            transient_status(status),
        ));
    }
    // Lossy decode, as the search path always has: a scraped page with a stray
    // byte should still parse.
    Ok(String::from_utf8_lossy(&resp.body).into_owned())
}
