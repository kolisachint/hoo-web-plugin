//! webtools as a hoocode extension component (`hoocode:extension@0.1.0`).
//!
//! Two tools, `fetch` and `search`, with the same argument names, defaults and
//! output as the webtools MCP server (`src/mcp.rs`). The libraries run over the
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
/// Per-request timeout caps, in seconds. With [`CALL_BUDGET`] they keep a fetch
/// (3 x timeout) and a search (3 attempts x 2 hops x timeout) inside the budget.
const FETCH_TIMEOUT_CAP_SECS: u64 = 8;
const SEARCH_TIMEOUT_CAP_SECS: u64 = 6;
/// The fallback provider is tried only when at least this much budget is left.
const FALLBACK_MIN_REMAINING: Duration = Duration::from_secs(8);

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
                name: "fetch".to_owned(),
                description: "Fetch a URL and return token-efficient, reference-style content. \
                    Links become inline [N] markers with full URLs in a references list. \
                    Handles HTML, JSON, and plain text."
                    .to_owned(),
                input_schema_json: fetch_schema().to_string(),
            },
            ToolInfo {
                name: "search".to_owned(),
                description: "Search the web and return results with reference-style URLs. \
                    Uses DuckDuckGo by default (no API key); a keyed provider can be configured."
                    .to_owned(),
                input_schema_json: search_schema().to_string(),
            },
        ]
    }

    fn call(name: String, args_json: String) -> Result<String, ToolError> {
        let result = match name.as_str() {
            "fetch" => {
                let args: FetchArgs = parse_args(&args_json)?;
                run_fetch(args)?
            }
            "search" => {
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
                "minimum": 0,
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
            "query": { "type": "string", "description": "Search query" },
            "max_results": { "type": "integer", "minimum": 1, "description": "Max results (default 5)" },
            "safe_search": { "type": "string", "enum": ["on", "off"], "description": "Safe search toggle" },
            "timeout": { "type": "integer", "minimum": 1, "description": "Request timeout in seconds (default 10, capped at 6). The whole search, fallback provider included, is limited to 25 s." },
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
    let content_type = match args.output.as_deref() {
        None => ContentType::Text,
        Some(s @ ("text" | "markdown" | "structured")) => ContentType::parse(s),
        Some(other) => {
            return Err(ToolError::InvalidArgs(format!(
                "output must be text, markdown or structured, not `{other}`"
            )))
        }
    };
    let timeout_secs = args.timeout.unwrap_or(10).clamp(1, FETCH_TIMEOUT_CAP_SECS);
    let options = FetchOptions {
        url: args.url.clone(),
        content_type,
        max_tokens: Some(args.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)),
        offset: args.offset.unwrap_or(0),
        outline: args.outline.unwrap_or(false),
        grep: args.grep,
        timeout_secs,
    };

    host::log(LogLevel::Debug, "fetch: requesting");
    let budget = Budget::new();
    let page = block_on(webfetch::fetch_with(&budget, &args.url, timeout_secs))
        .map_err(|e| ToolError::Failed(format!("{e:#}")))?;
    let result = webfetch::convert_page(page, &options);

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
    let provider = configured_provider("searchProvider").map_err(ToolError::Failed)?;
    let fallback = configured_fallback(&provider);

    let timeout_secs = args.timeout.unwrap_or(10).clamp(1, SEARCH_TIMEOUT_CAP_SECS);
    let max_results = Some(args.max_results.unwrap_or(5));
    let query = args.query;
    let options = |provider: Provider| SearchOptions {
        query: query.clone(),
        max_results,
        safe_search,
        timeout_secs,
        provider,
        ..Default::default()
    };

    // The library's own sequence (`run_search_with`), unrolled so the fallback
    // can be skipped when the budget is nearly spent.
    host::log(LogLevel::Debug, "search: querying");
    let budget = Budget::new();
    let mut result = block_on(websearch::run_search_with(&budget, options(provider)));
    if !answered(&result) {
        match fallback {
            Some(fb) if budget.remaining() >= FALLBACK_MIN_REMAINING => {
                let second = block_on(websearch::run_search_with(&budget, options(fb)));
                if answered(&second) {
                    result = second;
                }
            }
            Some(_) => host::log(
                LogLevel::Warn,
                "search: too little budget left for the fallback provider",
            ),
            None => {}
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
    fn new() -> Self {
        Self {
            inner: WasiTransport::new(),
            deadline: Instant::now() + CALL_BUDGET,
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
