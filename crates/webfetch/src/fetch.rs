use std::future::Future;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use url::Url;

#[cfg(feature = "native")]
use crate::guard;
#[cfg(feature = "native")]
use crate::tls::TlsConfig;
use webfetch_core::charset;
use webfetch_core::http::{status_text, transient_status};
#[cfg(feature = "native")]
use webfetch_core::transport::ReqwestTransport;
use webfetch_core::transport::{HttpRequest, Method, Transport};

const MAX_ATTEMPTS: u32 = 3;
const MAX_REDIRECTS: usize = 5;

/// Multiplier turning the per-request `--timeout` into a budget for the whole
/// fetch.
///
/// `--timeout` bounds one request. With retries and redirects a single fetch
/// could issue `MAX_ATTEMPTS * (MAX_REDIRECTS + 1)` requests, so `--timeout 10`
/// could keep running for minutes — not what anyone setting a timeout expects.
/// The whole fetch now shares one deadline, and each request gets whatever is
/// left of it.
const TOTAL_BUDGET_MULTIPLIER: u32 = 3;

/// Outcome of an HTTP fetch: the body, the URL we actually landed on after
/// following redirects, and the response's `Content-Type` (if any).
///
/// Serializable so a caller can hold one between runs — the CLI caches pages
/// this way, which is what lets a long document be paged without refetching it
/// per window. The library itself neither reads nor writes any such store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchedPage {
    pub body: String,
    pub final_url: String,
    pub content_type: Option<String>,
    /// Set when the page declared a charset this build cannot decode, so the
    /// body was read as UTF-8 and may be garbled.
    pub undecodable_charset: Option<String>,
}

/// One hop's result: either the final page, or a redirect to a raw `Location`.
enum Hop {
    Page(FetchedPage),
    Redirect(String),
}

/// Chooses the transport for one hop.
///
/// This is where a host-level policy attaches. The native [`fetch_page`] runs
/// the SSRF guard here and pins each hop's transport to the addresses it
/// resolved to. [`fetch_with`] uses the caller's transport unchanged, so a host
/// that enforces its own policy (or has none to enforce) skips the guard.
trait HopTransport: Send + Sync {
    type Transport: Transport;
    fn transport_for(
        &self,
        url: &Url,
    ) -> impl Future<Output = anyhow::Result<Self::Transport>> + Send;
}

/// The caller's transport, used for every hop as-is.
struct Fixed<'a, T>(&'a T);

impl<'a, T: Transport> HopTransport for Fixed<'a, T> {
    type Transport = &'a T;
    fn transport_for(
        &self,
        _url: &Url,
    ) -> impl Future<Output = anyhow::Result<Self::Transport>> + Send {
        let transport = self.0;
        async move { Ok(transport) }
    }
}

/// Native policy: validate the hop's host with the SSRF guard, then build a
/// transport pinned to the addresses that validation resolved.
///
/// Pinning closes the DNS-rebinding window between validation and connection.
/// It takes effect only on a direct connection: when `HTTP(S)_PROXY` is set, the
/// proxy resolves the host itself and the pinned addresses are never used. See
/// `docs/product.md`.
#[cfg(feature = "native")]
struct Guarded<'a> {
    tls: &'a TlsConfig,
}

#[cfg(feature = "native")]
impl HopTransport for Guarded<'_> {
    type Transport = ReqwestTransport;
    fn transport_for(
        &self,
        url: &Url,
    ) -> impl Future<Output = anyhow::Result<Self::Transport>> + Send {
        let tls = self.tls;
        async move {
            let pinned = guard::validate_url(url).await?;
            let host = url.host_str().unwrap_or("");
            Ok(ReqwestTransport::pinned(tls, host, &pinned)?)
        }
    }
}

/// The request for one page hop.
fn page_request(url: &Url, timeout: Duration) -> HttpRequest {
    HttpRequest {
        method: Method::Get,
        url: url.to_string(),
        headers: vec![
            (
                "Accept".to_string(),
                "text/html,application/xhtml+xml,*/*;q=0.8".to_string(),
            ),
            ("Accept-Language".to_string(), "en-US,en;q=0.9".to_string()),
        ],
        body: None,
        timeout: Some(timeout),
    }
}

/// One request attempt. The bool in the error reports whether the failure is
/// transient (worth retrying): connection/timeout errors, 5xx, and 429.
async fn attempt<T: Transport>(
    transport: &T,
    request: HttpRequest,
) -> Result<Hop, (anyhow::Error, bool)> {
    let url = request.url.clone();
    let resp = match transport.send(request).await {
        Ok(r) => r,
        Err(e) => {
            let transient = e.is_transient();
            return Err((e.into(), transient));
        }
    };

    let status = resp.status;

    // Redirects are surfaced to the caller (which re-validates and pins the
    // target): the transport never follows them.
    if (300..400).contains(&status) {
        return match resp.header("location") {
            Some(loc) => Ok(Hop::Redirect(loc.to_string())),
            None => Err((
                anyhow::anyhow!("redirect ({status}) without a Location header"),
                false,
            )),
        };
    }

    if !(200..300).contains(&status) {
        let kind = if status >= 500 {
            "server error"
        } else {
            "client error"
        };
        return Err((
            anyhow::anyhow!(
                "HTTP status {kind} ({}) for url ({url})",
                status_text(status)
            ),
            transient_status(status),
        ));
    }

    let content_type = resp.header("content-type").map(str::to_string);

    // Decode with the response's declared charset rather than assuming UTF-8:
    // a windows-1252 / ISO-8859-1 page is otherwise returned full of
    // replacement characters.
    let declared = content_type
        .as_deref()
        .and_then(charset::from_content_type)
        .or_else(|| charset::sniff_meta(&resp.body));
    let (body, undecodable_charset) = charset::decode(&resp.body, declared.as_deref());

    Ok(Hop::Page(FetchedPage {
        body,
        final_url: url,
        content_type,
        undecodable_charset,
    }))
}

/// Issue one hop's request, retrying transient failures with exponential
/// backoff (200ms, 400ms) while the overall deadline allows.
async fn fetch_with_retries<T: Transport>(
    transport: &T,
    request: &HttpRequest,
    deadline: Instant,
) -> anyhow::Result<Hop> {
    let mut delay = Duration::from_millis(200);
    for attempt_no in 1..=MAX_ATTEMPTS {
        match attempt(transport, request.clone()).await {
            Ok(hop) => return Ok(hop),
            Err((err, transient)) => {
                if attempt_no == MAX_ATTEMPTS || !transient {
                    return Err(err);
                }
                if Instant::now() + delay >= deadline {
                    return Err(err);
                }
                transport.sleep(delay).await;
                delay *= 2;
            }
        }
    }
    unreachable!("loop returns on the final attempt")
}

/// The redirect loop shared by both entry points. Each hop asks `hops` for its
/// transport, sends one request, and either returns the page or follows the
/// redirect. Capped at [`MAX_REDIRECTS`], and the whole operation at
/// [`TOTAL_BUDGET_MULTIPLIER`] times `timeout_secs`.
async fn fetch_hops<H: HopTransport>(
    hops: &H,
    url: &str,
    timeout_secs: u64,
) -> anyhow::Result<FetchedPage> {
    let per_request = Duration::from_secs(timeout_secs);
    let deadline = Instant::now() + per_request * TOTAL_BUDGET_MULTIPLIER;

    let mut current = Url::parse(url)?;
    let mut redirects = 0usize;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            anyhow::bail!(
                "fetch exceeded its total budget ({}s across redirects and retries)",
                timeout_secs * TOTAL_BUDGET_MULTIPLIER as u64
            );
        }

        let transport = hops.transport_for(&current).await?;
        let request = page_request(&current, per_request.min(remaining));

        match fetch_with_retries(&transport, &request, deadline).await? {
            Hop::Page(page) => return Ok(page),
            Hop::Redirect(location) => {
                redirects += 1;
                if redirects > MAX_REDIRECTS {
                    anyhow::bail!("too many redirects (>{MAX_REDIRECTS})");
                }
                current = current
                    .join(&location)
                    .map_err(|e| anyhow::anyhow!("invalid redirect target `{location}`: {e}"))?;
            }
        }
    }
}

/// Fetch a URL over the native transport, following redirects manually so the
/// SSRF guard re-validates and re-pins each hop (closing the DNS-rebinding
/// window for redirected hosts too), retrying transient failures with
/// exponential backoff.
#[cfg(feature = "native")]
pub async fn fetch_page(
    url: &str,
    timeout_secs: u64,
    tls: &TlsConfig,
) -> anyhow::Result<FetchedPage> {
    fetch_hops(&Guarded { tls }, url, timeout_secs).await
}

/// Fetch a URL through a caller-supplied transport.
///
/// Same redirect, retry and budget behaviour as [`fetch_page`], without the
/// SSRF guard: the transport is trusted to decide what it may reach.
pub async fn fetch_with(
    transport: &impl Transport,
    url: &str,
    timeout_secs: u64,
) -> anyhow::Result<FetchedPage> {
    fetch_hops(&Fixed(transport), url, timeout_secs).await
}
