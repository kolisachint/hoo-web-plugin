//! webtools as a hoocode extension component (`hoocode:extension@0.1.0`).
//!
//! Two tools, `WebFetch` and `WebSearch`, with the same argument names, defaults
//! and output as the webtools MCP server (`src/mcp.rs`). The host uses the names
//! as declared here, without a prefix. The libraries run over the
//! `wasi:http` transport, so every request goes through hoocode's permission gate
//! and SSRF block. Search keys come from the host's `config-get`.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use webfetch::types::{ContentStatus, ContentType, FetchOptions, FetchResult};
use webfetch_core::transport::{
    HttpRequest, HttpResponse, Transport, TransportError, WasiTransport,
};
use websearch::providers::Provider;
use websearch::types::{SearchOptions, SearchOutput};

wit_bindgen::generate!({
    path: "wit",
    world: "extension",
});

use hoocode::extension::host::{self, LogLevel};
use hoocode::extension::types::{ToolError, ToolInfo};

/// Output cap for `fetch` when the caller gives none. Same as the MCP server.
const DEFAULT_MAX_TOKENS: usize = 6_000;

/// Longest a whole tool call may run. hoocode traps a call at 30 s; the margin
/// covers conversion and rendering after the last request.
const CALL_BUDGET: Duration = Duration::from_secs(25);
/// Per-request timeout caps, in seconds. A fetch is at most three attempts at
/// one request, so `3 x cap` stays inside [`CALL_BUDGET`]. A search request is
/// capped at 12 s: Tavily answered in 4.5 to 5.3 s, which the old 6 s cap cut
/// off. Every request is also clamped to the time left in the call.
const FETCH_TIMEOUT_CAP_SECS: u64 = 8;
const SEARCH_TIMEOUT_CAP_SECS: u64 = 12;
/// The fallback provider is tried only when at least this much budget is left,
/// and the primary never runs past `CALL_BUDGET - FALLBACK_MIN_REMAINING` when
/// a fallback is configured (see [`primary_limit`]).
const FALLBACK_MIN_REMAINING: Duration = Duration::from_secs(8);
/// Slack past the primary's limit. A primary that hits its deadline returns a
/// little after it, so without this the fallback would see just under
/// [`FALLBACK_MIN_REMAINING`] left and be skipped exactly when the primary
/// used its whole allowance.
const PRIMARY_SLACK: Duration = Duration::from_secs(1);

struct Webtools;

export!(Webtools);

/// `fetch` arguments. Field names and defaults match the MCP `fetch` tool.
#[derive(Deserialize)]
struct FetchArgs {
    url: String,
    output: Option<String>,
    max_tokens: Option<usize>,
    offset: Option<usize>,
    outline: Option<bool>,
    grep: Option<String>,
    timeout: Option<u64>,
    json: Option<bool>,
}

/// `search` arguments. Field names and defaults match the MCP `search` tool.
#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    max_results: Option<usize>,
    safe_search: Option<String>,
    timeout: Option<u64>,
    json: Option<bool>,
}

impl exports::hoocode::extension::tool::Guest for Webtools {
    fn list_tools() -> Vec<ToolInfo> {
        vec![
            ToolInfo {
                name: "WebFetch".to_owned(),
                description: "Fetch a URL and return token-efficient, reference-style content. \
                    Links become inline [N] markers with full URLs in a references list. \
                    Handles HTML, JSON, and plain text."
                    .to_owned(),
                input_schema_json: fetch_schema().to_string(),
            },
            ToolInfo {
                name: "WebSearch".to_owned(),
                description: "Search the web and return results with reference-style URLs. \
                    Uses DuckDuckGo by default (no API key); a keyed provider can be configured."
                    .to_owned(),
                input_schema_json: search_schema().to_string(),
            },
        ]
    }

    fn call(name: String, args_json: String) -> Result<String, ToolError> {
        let result = match name.as_str() {
            "WebFetch" => {
                let args: FetchArgs = parse_args(&args_json)?;
                run_fetch(args)?
            }
            "WebSearch" => {
                let args: SearchArgs = parse_args(&args_json)?;
                run_search(args)?
            }
            _ => return Err(ToolError::UnknownTool(name)),
        };
        Ok(result.to_string())
    }
}

fn parse_args<T: for<'de> Deserialize<'de>>(args_json: &str) -> Result<T, ToolError> {
    serde_json::from_str(args_json).map_err(|e| ToolError::InvalidArgs(e.to_string()))
}

fn fetch_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "url": { "type": "string", "description": "URL to fetch" },
            "output": {
                "type": "string",
                "enum": ["text", "markdown", "structured"],
                "description": "Output format (default text)"
            },
            "max_tokens": {
                "type": "integer",
                "minimum": 1,
                "description": "Cap on output size in estimated tokens (default 6000)"
            },
            "offset": {
                "type": "integer",
                "minimum": 0,
                "description": "Byte offset into the extracted text to start from, for reading a long page one window at a time. Use the next_offset reported by the previous call; windows tile the document exactly."
            },
            "outline": {
                "type": "boolean",
                "description": "Return the page's headings and the offset that reads each section, instead of the body. Cheap map of a long page: read it first, then fetch the one section you need at its offset."
            },
            "grep": {
                "type": "string",
                "description": "Return where the page matches this regular expression — offset, surrounding text, and the section it falls in — instead of the body. Case-insensitive unless the pattern carries an uppercase letter."
            },
            "timeout": { "type": "integer", "minimum": 1, "description": "Request timeout in seconds (default 10, capped at 8). The whole fetch, redirects and retries included, is limited to 25 s." },
            "json": {
                "type": "boolean",
                "description": "Return the full FetchResult as JSON instead of the compact form (default false)"
            }
        },
        "required": ["url"]
    })
}

fn search_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "minLength": 1, "description": "Search query. Must not be empty or only whitespace." },
            "max_results": { "type": "integer", "minimum": 1, "description": "Max results (default 5)" },
            "safe_search": { "type": "string", "enum": ["on", "off"], "description": "Safe search toggle" },
            "timeout": { "type": "integer", "minimum": 1, "description": "Request timeout in seconds (default 12, capped at 12). The whole search, fallback provider included, is limited to 25 s; a fallback is tried only with 8 s or more left." },
            "json": {
                "type": "boolean",
                "description": "Return the full SearchOutput as JSON instead of the compact form (default false)"
            }
        },
        "required": ["query"]
    })
}

/// Fetch one page and convert it. Compact output is `{url, final_url, title?,
/// content, truncated, next_offset?}`; `json: true` returns the whole FetchResult.
/// A page that extracted no content is a failure, as it is in the MCP server.
fn run_fetch(args: FetchArgs) -> Result<Value, ToolError> {
    let options = fetch_options(&args)?;

    host::log(LogLevel::Debug, "fetch: requesting");
    let budget = Budget::new(CALL_BUDGET);
    let page = block_on(webfetch::fetch_with(
        &budget,
        &args.url,
        options.timeout_secs,
    ))
    .map_err(|e| ToolError::Failed(format!("{e:#}")))?;
    // An offset past the end of the page is only known once it is converted.
    let result = webfetch::convert_page(page, &options).map_err(invalid_args)?;

    if result.status.is_failure() {
        return Err(ToolError::Failed(failure_message(&result)));
    }
    if args.json.unwrap_or(false) {
        return serde_json::to_value(&result).map_err(|e| ToolError::Failed(e.to_string()));
    }

    let mut out = json!({
        "url": args.url,
        "final_url": result.final_url,
        "content": result.content,
        "truncated": result.truncated,
    });
    if !result.title.is_empty() {
        out["title"] = json!(result.title);
    }
    if let Some(next) = result.next_offset {
        out["next_offset"] = json!(next);
    }
    Ok(out)
}

/// The `fetch` arguments as conversion options. Checked here, before any request,
/// so a bad argument costs no download.
fn fetch_options(args: &FetchArgs) -> Result<FetchOptions, ToolError> {
    let content_type = match args.output.as_deref() {
        None => ContentType::Text,
        Some(s @ ("text" | "markdown" | "structured")) => ContentType::parse(s),
        Some(other) => {
            return Err(ToolError::InvalidArgs(format!(
                "output must be text, markdown or structured, not `{other}`"
            )))
        }
    };
    let options = FetchOptions {
        url: args.url.clone(),
        content_type,
        max_tokens: Some(args.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)),
        offset: args.offset.unwrap_or(0),
        outline: args.outline.unwrap_or(false),
        grep: args.grep.clone(),
        timeout_secs: args.timeout.unwrap_or(10).clamp(1, FETCH_TIMEOUT_CAP_SECS),
    };
    options.validate().map_err(invalid_args)?;
    Ok(options)
}

/// A library refusal of the arguments, reported to the host as `invalid-args`.
fn invalid_args(error: impl std::fmt::Display) -> ToolError {
    ToolError::InvalidArgs(error.to_string())
}

/// The message for a page that produced no usable content: the MCP server's
/// wording, plus whatever body there was.
fn failure_message(result: &FetchResult) -> String {
    let note = result.status.note().unwrap_or("no content extracted");
    match result.status {
        ContentStatus::Ok => String::new(),
        _ if result.content.is_empty() => format!("[{note}]"),
        _ => format!("{}\n\n[{note}]", result.content),
    }
}

/// Run a search through the provider the host configures. Compact output is
/// `{query, provider, result_count, content}`; `json: true` returns the whole
/// SearchOutput. A blocked search is a failure, not an empty result.
fn run_search(args: SearchArgs) -> Result<Value, ToolError> {
    let base = search_options(&args)?;
    let provider = configured_provider("searchProvider").map_err(ToolError::Failed)?;
    let fallback = configured_fallback(&provider);

    let options = |provider: Provider| SearchOptions {
        provider,
        ..base.clone()
    };

    // The library's own sequence (`run_search_with`), unrolled so the fallback
    // can be skipped when the budget is nearly spent. The primary gets the time
    // the fallback does not reserve, so a slow or retrying primary cannot leave
    // the fallback with nothing.
    host::log(LogLevel::Debug, "search: querying");
    let started = Instant::now();
    let primary_budget = Budget::new(primary_limit(fallback.is_some()));
    let mut result = block_on(websearch::run_search_with(
        &primary_budget,
        options(provider),
    ));
    if !answered(&result) {
        if let Some(fb) = fallback {
            let elapsed = started.elapsed();
            if fallback_allowed(elapsed) {
                let fallback_budget = Budget::new(CALL_BUDGET.saturating_sub(elapsed));
                let second = block_on(websearch::run_search_with(&fallback_budget, options(fb)));
                if answered(&second) {
                    result = second;
                }
            } else {
                host::log(
                    LogLevel::Warn,
                    "search: too little budget left for the fallback provider",
                );
            }
        }
    }
    let output = result.map_err(|e| ToolError::Failed(format!("{e:#}")))?;

    let rendered = websearch::render_output(&output);
    if output.status.is_failure() {
        return Err(ToolError::Failed(rendered));
    }
    if args.json.unwrap_or(false) {
        return serde_json::to_value(&output).map_err(|e| ToolError::Failed(e.to_string()));
    }
    Ok(json!({
        "query": output.query,
        "provider": output.provider,
        "result_count": output.result_count,
        "content": rendered,
    }))
}

/// The `search` arguments as search options, without a provider. Checked here,
/// before the host is asked for a provider or any request is made.
fn search_options(args: &SearchArgs) -> Result<SearchOptions, ToolError> {
    let safe_search = match args.safe_search.as_deref() {
        None => None,
        Some("on") => Some(true),
        Some("off") => Some(false),
        Some(other) => {
            return Err(ToolError::InvalidArgs(format!(
                "safe_search must be on or off, not `{other}`"
            )))
        }
    };
    let options = SearchOptions {
        query: args.query.clone(),
        max_results: Some(args.max_results.unwrap_or(5)),
        safe_search,
        timeout_secs: args
            .timeout
            .unwrap_or(SEARCH_TIMEOUT_CAP_SECS)
            .clamp(1, SEARCH_TIMEOUT_CAP_SECS),
        ..Default::default()
    };
    options.validate().map_err(invalid_args)?;
    Ok(options)
}

/// How long the primary search may run. With a fallback configured, the last
/// [`FALLBACK_MIN_REMAINING`] of the call, less [`PRIMARY_SLACK`], is kept for
/// it. Without one, the primary may use the whole call.
fn primary_limit(has_fallback: bool) -> Duration {
    if has_fallback {
        CALL_BUDGET.saturating_sub(FALLBACK_MIN_REMAINING + PRIMARY_SLACK)
    } else {
        CALL_BUDGET
    }
}

/// Whether the fallback still gets its minimum once `elapsed` has passed since
/// the call started.
fn fallback_allowed(elapsed: Duration) -> bool {
    CALL_BUDGET.saturating_sub(elapsed) >= FALLBACK_MIN_REMAINING
}

/// True when a provider gave an answer: no error and no blocked or empty status.
fn answered<E>(result: &Result<SearchOutput, E>) -> bool {
    matches!(result, Ok(out) if !out.status.is_failure())
}

/// The WASI transport under one deadline for the whole tool call. Each request
/// gets the time left, and once the deadline has passed requests fail at once
/// and backoff sleeps are cut short, so no retry or fallback runs past the budget.
struct Budget {
    inner: WasiTransport,
    deadline: Instant,
}

impl Budget {
    /// A budget that lets its requests run for `limit` from now.
    fn new(limit: Duration) -> Self {
        Self {
            inner: WasiTransport::new(),
            deadline: Instant::now() + limit,
        }
    }

    fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

impl Transport for Budget {
    async fn send(&self, mut req: HttpRequest) -> Result<HttpResponse, TransportError> {
        let left = self.remaining();
        if left.is_zero() {
            return Err(TransportError::Timeout(req.url.clone()));
        }
        req.timeout = Some(req.timeout.map_or(left, |t| t.min(left)));
        self.inner.send(req).await
    }

    async fn sleep(&self, d: Duration) {
        self.inner.sleep(d.min(self.remaining())).await;
    }
}

/// A non-empty setting from the host, or `None`.
fn setting(key: &str) -> Option<String> {
    host::config_get(key).filter(|v| !v.trim().is_empty())
}

/// A required setting, with the key named in the error.
fn required(key: &str) -> Result<String, String> {
    setting(key).ok_or_else(|| format!("search needs the `{key}` setting"))
}

/// Build a provider from its name, reading its credential from the host.
/// Mirrors `src/config.rs` (`build`): the same names and the same keys, in camelCase.
fn provider_named(name: &str) -> Result<Provider, String> {
    let label =
        Provider::parse_name(name).ok_or_else(|| format!("unknown search provider `{name}`"))?;
    Ok(match label {
        "duckduckgo" => Provider::Duckduckgo,
        "brave" => Provider::Brave {
            api_key: required("braveApiKey")?,
        },
        "tavily" => Provider::Tavily {
            api_key: required("tavilyApiKey")?,
        },
        "searxng" => Provider::Searxng {
            base_url: required("searxngUrl")?,
            api_key: setting("searxngApiKey"),
        },
        _ => return Err(format!("unknown search provider `{name}`")),
    })
}

/// The primary provider: `searchProvider`, or keyless DuckDuckGo when unset.
fn configured_provider(key: &str) -> Result<Provider, String> {
    match setting(key) {
        Some(name) => provider_named(&name),
        None => Ok(Provider::Duckduckgo),
    }
}

/// The fallback from `searchFallback`. A fallback that is unset, `none`, the same
/// as the primary, or unusable is dropped: it is a safety net, not the request.
fn configured_fallback(primary: &Provider) -> Option<Provider> {
    let name = setting("searchFallback")?;
    if name.trim().eq_ignore_ascii_case("none") {
        return None;
    }
    match provider_named(&name) {
        Ok(p) if p.label() != primary.label() => Some(p),
        Ok(_) => None,
        Err(reason) => {
            host::log(
                LogLevel::Warn,
                &format!("search fallback dropped: {reason}"),
            );
            None
        }
    }
}

/// Drive a future to completion on this thread. The `wasi:http` transport
/// completes each request synchronously, so the first poll is normally ready;
/// the loop only repeats if a future ever returns `Pending`.
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exports::hoocode::extension::tool::Guest;

    /// The names the model sees: as declared, PascalCase, matching the regex the
    /// host accepts. Written out by hand so a rename cannot pass unnoticed.
    #[test]
    fn list_tools_declares_web_fetch_and_web_search() {
        let tools = <Webtools as Guest>::list_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["WebFetch", "WebSearch"]);

        for name in names {
            let mut chars = name.chars();
            assert!(
                chars.next().is_some_and(|c| c.is_ascii_alphabetic()),
                "{name}"
            );
            assert!(name.len() <= 64, "{name}");
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "{name}"
            );
        }
    }

    /// Dispatch uses the same names. The old names are unknown now. The calls
    /// below fail on the empty arguments before any request is made.
    #[test]
    fn call_dispatches_by_declared_name_only() {
        for old in ["fetch", "search"] {
            assert!(
                matches!(
                    <Webtools as Guest>::call(old.to_owned(), "{}".to_owned()),
                    Err(ToolError::UnknownTool(_))
                ),
                "{old}"
            );
        }
        assert!(matches!(
            <Webtools as Guest>::call("WebFetch".to_owned(), "{}".to_owned()),
            Err(ToolError::InvalidArgs(_))
        ));
        assert!(matches!(
            <Webtools as Guest>::call("WebSearch".to_owned(), "{}".to_owned()),
            Err(ToolError::InvalidArgs(_))
        ));
    }

    fn fetch_args(json: &str) -> FetchArgs {
        serde_json::from_str(json).expect("fetch args")
    }

    fn search_args(json: &str) -> SearchArgs {
        serde_json::from_str(json).expect("search args")
    }

    fn is_invalid<T>(result: &Result<T, ToolError>) -> bool {
        matches!(result, Err(ToolError::InvalidArgs(_)))
    }

    #[test]
    fn a_zero_token_budget_is_invalid_args() {
        let args = fetch_args(r#"{"url":"https://x.test/","max_tokens":0}"#);
        assert!(is_invalid(&fetch_options(&args)));

        let args = fetch_args(r#"{"url":"https://x.test/","max_tokens":1}"#);
        assert_eq!(fetch_options(&args).unwrap().max_tokens, Some(1));
    }

    #[test]
    fn a_zero_offset_is_valid_and_so_is_a_later_one() {
        let args = fetch_args(r#"{"url":"https://x.test/","offset":0}"#);
        assert_eq!(fetch_options(&args).unwrap().offset, 0);

        let args = fetch_args(r#"{"url":"https://x.test/","offset":4096}"#);
        assert_eq!(fetch_options(&args).unwrap().offset, 4096);
    }

    #[test]
    fn an_invalid_grep_pattern_is_invalid_args_before_any_request() {
        let args = fetch_args(r#"{"url":"https://x.test/","grep":"([a-"}"#);
        match fetch_options(&args) {
            Err(ToolError::InvalidArgs(msg)) => assert!(msg.contains("([a-"), "{msg}"),
            other => panic!("expected invalid-args, got {:?}", other.is_ok()),
        }

        let args = fetch_args(r#"{"url":"https://x.test/","grep":"rate limit"}"#);
        assert!(fetch_options(&args).is_ok());
    }

    #[test]
    fn an_unknown_output_format_is_invalid_args() {
        let args = fetch_args(r#"{"url":"https://x.test/","output":"pdf"}"#);
        assert!(is_invalid(&fetch_options(&args)));
    }

    #[test]
    fn a_fetch_timeout_is_capped_and_at_least_one_second() {
        let args = fetch_args(r#"{"url":"https://x.test/","timeout":99}"#);
        assert_eq!(
            fetch_options(&args).unwrap().timeout_secs,
            FETCH_TIMEOUT_CAP_SECS
        );

        // Unset, the default of 10 is clamped to the cap, as the docs say.
        let args = fetch_args(r#"{"url":"https://x.test/"}"#);
        assert_eq!(
            fetch_options(&args).unwrap().timeout_secs,
            FETCH_TIMEOUT_CAP_SECS
        );
    }

    #[test]
    fn an_empty_or_blank_search_query_is_invalid_args() {
        for query in ["", "   ", "\t\n"] {
            let args = search_args(&format!(r#"{{"query":{query:?}}}"#));
            assert!(is_invalid(&search_options(&args)), "{query:?}");
        }
        let args = search_args(r#"{"query":"rust async"}"#);
        assert_eq!(search_options(&args).unwrap().query, "rust async");
    }

    #[test]
    fn a_search_timeout_default_and_cap_are_twelve_seconds() {
        let args = search_args(r#"{"query":"x"}"#);
        assert_eq!(search_options(&args).unwrap().timeout_secs, 12);

        let args = search_args(r#"{"query":"x","timeout":60}"#);
        assert_eq!(search_options(&args).unwrap().timeout_secs, 12);

        // A 5.3 s Tavily answer, the case the old 6 s cap was too close to.
        let args = search_args(r#"{"query":"x","timeout":6}"#);
        assert_eq!(search_options(&args).unwrap().timeout_secs, 6);
    }

    #[test]
    fn a_bad_safe_search_value_is_invalid_args() {
        let args = search_args(r#"{"query":"x","safe_search":"maybe"}"#);
        assert!(is_invalid(&search_options(&args)));
    }

    /// The primary and the fallback share one call. The fallback's reserve is
    /// kept whole, and a primary that runs its full cap fits beside it.
    #[test]
    fn the_primary_leaves_the_fallback_its_reserve() {
        assert_eq!(
            primary_limit(true) + FALLBACK_MIN_REMAINING + PRIMARY_SLACK,
            CALL_BUDGET,
            "primary, slack and fallback reserve must add up to the call budget"
        );
        assert!(primary_limit(true) >= Duration::from_secs(SEARCH_TIMEOUT_CAP_SECS));
        assert_eq!(primary_limit(false), CALL_BUDGET);
    }

    /// A primary that uses its whole limit and overshoots it a little (its
    /// deadline is checked between requests) must still leave the fallback
    /// its minimum. Before the slack, this exact case skipped the fallback.
    #[test]
    fn a_primary_that_overshoots_its_limit_still_leaves_the_fallback_time() {
        let overshoot = Duration::from_millis(300);
        assert!(fallback_allowed(primary_limit(true)));
        assert!(fallback_allowed(primary_limit(true) + overshoot));
        assert!(!fallback_allowed(CALL_BUDGET - FALLBACK_MIN_REMAINING + overshoot));
    }

    #[test]
    fn the_schema_states_the_minimums() {
        let fetch = fetch_schema();
        assert_eq!(fetch["properties"]["max_tokens"]["minimum"], 1);
        assert_eq!(fetch["properties"]["offset"]["minimum"], 0);

        let search = search_schema();
        assert_eq!(search["properties"]["query"]["minLength"], 1);
        assert!(search["properties"]["timeout"]["description"]
            .as_str()
            .unwrap()
            .contains("capped at 12"));
    }
}
