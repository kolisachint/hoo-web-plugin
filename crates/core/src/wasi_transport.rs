//! WASI 0.2 `wasi:http` transport.
//!
//! For a WebAssembly component whose host provides `wasi:http/outgoing-handler`.
//! The host owns the network policy, so this transport does not check the
//! target host (no SSRF guard), does not pin addresses and configures no TLS
//! trust. The caller's host must enforce those itself. Its refusals surface as
//! `TransportError::Other("denied by host: ...")`.
//!
//! Every call is synchronous and takes no `.await` while a WASI resource is
//! alive. That keeps the futures `Send` even though WASI resources are not.
//! Waiting is done with `poll` over the operation's pollable and a monotonic
//! timer, so one `timeout` bounds the whole request: the connect, the headers
//! and the body together.

use std::time::Duration;

use url::Url;
use wasip2::clocks::monotonic_clock;
use wasip2::http::outgoing_handler;
use wasip2::http::types::{
    ErrorCode, Fields, IncomingBody, Method as WasiMethod, OutgoingBody, OutgoingRequest,
    RequestOptions, Scheme,
};
use wasip2::io::poll::{self, Pollable};
use wasip2::io::streams::{InputStream, OutputStream, StreamError};

use crate::http::{push_capped, ERROR_BODY_CAP, MAX_BODY_BYTES, USER_AGENT};
use crate::transport::{HttpRequest, HttpResponse, Method, Transport, TransportError};

/// Most bytes one `read` call asks for.
const READ_CHUNK: u64 = 64 * 1024;

/// `blocking-write-and-flush` accepts at most this many bytes per call.
const WRITE_CHUNK: usize = 4096;

/// Sends requests through `wasi:http/outgoing-handler`. Holds no state; the
/// host's handler is reached through the WASI imports.
#[derive(Debug, Clone, Copy, Default)]
pub struct WasiTransport;

impl WasiTransport {
    pub fn new() -> Self {
        Self
    }
}

impl Transport for WasiTransport {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, TransportError> {
        send_blocking(req)
    }

    async fn sleep(&self, d: Duration) {
        monotonic_clock::subscribe_duration(nanos(d)).block();
    }
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

fn other(msg: &str) -> TransportError {
    TransportError::Other(msg.to_string())
}

/// Wait until `pollable` is ready or `deadline` (monotonic nanoseconds) passes.
/// Returns whether the pollable is ready. With no deadline it waits unbounded.
/// The timer has no parent resource. Callers drop their own pollable before the
/// resource it came from.
fn wait_ready(pollable: &Pollable, deadline: Option<u64>) -> bool {
    let Some(deadline) = deadline else {
        pollable.block();
        return true;
    };
    if pollable.ready() {
        return true;
    }
    let timer = monotonic_clock::subscribe_instant(deadline);
    let _ = poll::poll(&[pollable, &timer]);
    pollable.ready()
}

/// One request, one hop. The whole exchange, including the body read, runs
/// without yielding to an executor, and stops at the request's timeout.
fn send_blocking(req: HttpRequest) -> Result<HttpResponse, TransportError> {
    let deadline = req
        .timeout
        .map(|t| monotonic_clock::now().saturating_add(nanos(t)));

    let url = Url::parse(&req.url)
        .map_err(|e| TransportError::Other(format!("invalid URL `{}`: {e}", req.url)))?;
    let scheme = match url.scheme() {
        "https" => Scheme::Https,
        "http" => Scheme::Http,
        unknown => {
            return Err(TransportError::Other(format!(
                "unsupported URL scheme `{unknown}`"
            )))
        }
    };
    let host = url.host_str().ok_or_else(|| other("URL has no host"))?;
    let authority = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
    let path_with_query = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    };

    let mut headers: Vec<(String, Vec<u8>)> = req
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), v.clone().into_bytes()))
        .collect();
    if !req
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
    {
        headers.push(("user-agent".to_string(), USER_AGENT.as_bytes().to_vec()));
    }
    let fields = Fields::from_list(&headers)
        .map_err(|e| TransportError::Other(format!("invalid request header: {e}")))?;

    let request = OutgoingRequest::new(fields);
    request
        .set_method(&wasi_method(req.method))
        .map_err(|()| other("host rejected the request method"))?;
    request
        .set_scheme(Some(&scheme))
        .map_err(|()| other("host rejected the request scheme"))?;
    request
        .set_authority(Some(&authority))
        .map_err(|()| other("host rejected the request authority"))?;
    request
        .set_path_with_query(Some(&path_with_query))
        .map_err(|()| other("host rejected the request path"))?;

    let out_body = request
        .body()
        .map_err(|()| other("cannot open the request body"))?;
    if let Some(bytes) = &req.body {
        write_body(&out_body, bytes)?;
    }
    OutgoingBody::finish(out_body, None).map_err(|c| map_error_code(c, &req.url))?;

    let options = request_options(req.timeout)?;
    let future =
        outgoing_handler::handle(request, options).map_err(|c| map_error_code(c, &req.url))?;
    let ready = {
        let pollable = future.subscribe();
        wait_ready(&pollable, deadline)
    };
    if !ready {
        // Dropping `future` cancels the exchange on the host side.
        return Err(TransportError::Timeout(req.url.clone()));
    }
    let response = match future.get() {
        Some(Ok(Ok(resp))) => resp,
        Some(Ok(Err(code))) => return Err(map_error_code(code, &req.url)),
        Some(Err(())) => return Err(other("response was already taken")),
        None => return Err(other("response not ready after waiting on it")),
    };

    let status = response.status();
    let resp_headers = response
        .headers()
        .entries()
        .into_iter()
        .filter_map(|(name, value)| {
            Some((name.to_ascii_lowercase(), String::from_utf8(value).ok()?))
        })
        .collect();

    let incoming = response
        .consume()
        .map_err(|()| other("response body was already taken"))?;
    // A redirect's body is never used, so it is not read. Error bodies are
    // kept only for the message, so they get the smaller cap.
    let body = match status {
        300..=399 => Vec::new(),
        400..=599 => read_body(&incoming, &req.url, deadline, ERROR_BODY_CAP)?,
        _ => read_body(&incoming, &req.url, deadline, MAX_BODY_BYTES)?,
    };
    // The stream is a child of the body and is dropped by `read_body`, so the
    // body can now be finished.
    let _ = IncomingBody::finish(incoming);

    Ok(HttpResponse {
        status,
        headers: resp_headers,
        body,
    })
}

fn wasi_method(method: Method) -> WasiMethod {
    match method {
        Method::Get => WasiMethod::Get,
        Method::Post => WasiMethod::Post,
    }
}

/// Connect, first-byte and between-bytes limits all take the request timeout,
/// so the host also gives up early. The whole-request bound is enforced by
/// [`wait_ready`] against the deadline, not by these limits.
fn request_options(timeout: Option<Duration>) -> Result<Option<RequestOptions>, TransportError> {
    let Some(timeout) = timeout else {
        return Ok(None);
    };
    let options = RequestOptions::new();
    let limit = Some(nanos(timeout));
    options
        .set_connect_timeout(limit)
        .map_err(|()| other("host rejected the connect timeout"))?;
    options
        .set_first_byte_timeout(limit)
        .map_err(|()| other("host rejected the first-byte timeout"))?;
    options
        .set_between_bytes_timeout(limit)
        .map_err(|()| other("host rejected the between-bytes timeout"))?;
    Ok(Some(options))
}

/// Write a request body in host-sized pieces, then the caller finishes it.
fn write_body(body: &OutgoingBody, bytes: &[u8]) -> Result<(), TransportError> {
    let stream: OutputStream = body
        .write()
        .map_err(|()| other("cannot write the request body"))?;
    for chunk in bytes.chunks(WRITE_CHUNK) {
        stream
            .blocking_write_and_flush(chunk)
            .map_err(|e| TransportError::Other(format!("writing the request body: {e:?}")))?;
    }
    Ok(())
}

/// Read a response body under `cap`, waiting for each chunk against the
/// request deadline. Oversized bodies are truncated, not rejected, as on the
/// native transport.
fn read_body(
    body: &IncomingBody,
    target: &str,
    deadline: Option<u64>,
    cap: usize,
) -> Result<Vec<u8>, TransportError> {
    let stream: InputStream = body
        .stream()
        .map_err(|()| other("response body stream is unavailable"))?;
    let mut buf = Vec::new();
    loop {
        let ready = {
            let pollable = stream.subscribe();
            wait_ready(&pollable, deadline)
        };
        if !ready {
            return Err(TransportError::Timeout(target.to_string()));
        }
        match stream.read(READ_CHUNK) {
            Ok(chunk) => {
                if push_capped(&mut buf, &chunk, cap) {
                    break;
                }
            }
            Err(StreamError::Closed) => break,
            Err(StreamError::LastOperationFailed(e)) => {
                return Err(TransportError::Other(format!(
                    "reading the response body: {e}"
                )))
            }
        }
    }
    Ok(buf)
}

/// Map a host error onto the transport's categories. Timeouts and connection
/// failures are transient; TLS and everything else are final.
fn map_error_code(code: ErrorCode, target: &str) -> TransportError {
    let detail = format!("{code:?}");
    match code {
        ErrorCode::DnsTimeout
        | ErrorCode::ConnectionTimeout
        | ErrorCode::ConnectionReadTimeout
        | ErrorCode::ConnectionWriteTimeout
        | ErrorCode::HttpResponseTimeout => TransportError::Timeout(target.to_string()),
        ErrorCode::DnsError(_)
        | ErrorCode::DestinationNotFound
        | ErrorCode::DestinationUnavailable
        | ErrorCode::DestinationIpUnroutable
        | ErrorCode::ConnectionRefused
        | ErrorCode::ConnectionTerminated
        | ErrorCode::ConnectionLimitReached => TransportError::Connect(detail),
        ErrorCode::TlsProtocolError
        | ErrorCode::TlsCertificateError
        | ErrorCode::TlsAlertReceived(_) => TransportError::Tls(detail),
        ErrorCode::DestinationIpProhibited => other("denied by host: destination IP prohibited"),
        ErrorCode::HttpRequestDenied => other("denied by host: request denied"),
        _ => TransportError::Other(detail),
    }
}
