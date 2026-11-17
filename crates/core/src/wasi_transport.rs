//! WASI 0.2 `wasi:http` transport.
//!
//! For a WebAssembly component whose host provides `wasi:http/outgoing-handler`.
//! The host owns the network policy, so this transport does not check the
//! target host (no SSRF guard), does not pin addresses and configures no TLS
//! trust. The caller's host must enforce those itself. Its refusals surface as
//! `TransportError::Other("denied by host: ...")`.
//!
//! Every call is synchronous. Pollables are waited on with `.block()`, and no
//! `.await` is taken while a WASI resource is alive. That keeps the futures
//! `Send` even though WASI resources are not.

use std::time::{Duration, Instant};

use url::Url;
use wasip2::clocks::monotonic_clock;
use wasip2::http::outgoing_handler;
use wasip2::http::types::{
    ErrorCode, Fields, IncomingBody, Method as WasiMethod, OutgoingBody, OutgoingRequest,
    RequestOptions, Scheme,
};
use wasip2::io::streams::{InputStream, OutputStream, StreamError};

use crate::http::{push_capped, MAX_BODY_BYTES, USER_AGENT};
use crate::transport::{HttpRequest, HttpResponse, Method, Transport, TransportError};

/// Most bytes one `blocking-read` call asks for.
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

/// One request, one hop. The whole exchange, including the body read, runs
/// without yielding to an executor.
fn send_blocking(req: HttpRequest) -> Result<HttpResponse, TransportError> {
    let deadline = req.timeout.map(|t| Instant::now() + t);

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
    OutgoingBody::finish(out_body, None).map_err(map_error_code)?;

    let options = request_options(req.timeout)?;
    let future = outgoing_handler::handle(request, options).map_err(map_error_code)?;
    future.subscribe().block();
    let response = match future.get() {
        Some(Ok(Ok(resp))) => resp,
        Some(Ok(Err(code))) => return Err(map_error_code(code)),
        Some(Err(())) => return Err(other("response was already taken")),
        None => return Err(other("response not ready after blocking on it")),
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
    let body = read_body(&incoming, deadline)?;
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

/// Connect, first-byte and between-bytes limits all take the request timeout.
/// `wasi:http` has no single whole-request limit, so [`read_body`] also checks
/// the deadline between chunks.
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

/// Read a response body under the same cap as the native transport:
/// oversized bodies are truncated, not rejected.
fn read_body(body: &IncomingBody, deadline: Option<Instant>) -> Result<Vec<u8>, TransportError> {
    let stream: InputStream = body
        .stream()
        .map_err(|()| other("response body stream is unavailable"))?;
    let mut buf = Vec::new();
    loop {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(TransportError::Timeout);
        }
        match stream.blocking_read(READ_CHUNK) {
            Ok(chunk) => {
                if push_capped(&mut buf, &chunk, MAX_BODY_BYTES) {
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
fn map_error_code(code: ErrorCode) -> TransportError {
    let detail = format!("{code:?}");
    match code {
        ErrorCode::DnsTimeout
        | ErrorCode::ConnectionTimeout
        | ErrorCode::ConnectionReadTimeout
        | ErrorCode::ConnectionWriteTimeout
        | ErrorCode::HttpResponseTimeout => TransportError::Timeout,
        ErrorCode::DnsError(_)
        | ErrorCode::DestinationNotFound
        | ErrorCode::DestinationUnavailable
        | ErrorCode::DestinationIpProhibited
        | ErrorCode::DestinationIpUnroutable
        | ErrorCode::ConnectionRefused
        | ErrorCode::ConnectionTerminated
        | ErrorCode::ConnectionLimitReached => TransportError::Connect(detail),
        ErrorCode::TlsProtocolError
        | ErrorCode::TlsCertificateError
        | ErrorCode::TlsAlertReceived(_) => TransportError::Tls(detail),
        ErrorCode::HttpRequestDenied => {
            TransportError::Other("denied by host: request denied".to_string())
        }
        _ => TransportError::Other(detail),
    }
}
