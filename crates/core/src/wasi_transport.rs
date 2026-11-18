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

use std::io::{self, Read};
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

use crate::content_encoding::{self, Coding};
use crate::http::{ERROR_BODY_CAP, MAX_BODY_BYTES, USER_AGENT};
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
    if !req
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("accept-encoding"))
    {
        headers.push((
            "accept-encoding".to_string(),
            content_encoding::ACCEPT_ENCODING.as_bytes().to_vec(),
        ));
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
    // Read before the header list is built below, which drops non-UTF-8 values.
    let encodings: Vec<String> = response
        .headers()
        .get("content-encoding")
        .iter()
        .map(|value| String::from_utf8_lossy(value).into_owned())
        .collect();
    let coding = content_encoding::coding_of(encodings.iter().map(String::as_str));
    // The body is decoded below, so the header no longer describes it.
    let resp_headers = response
        .headers()
        .entries()
        .into_iter()
        .filter_map(|(name, value)| {
            Some((name.to_ascii_lowercase(), String::from_utf8(value).ok()?))
        })
        .filter(|(name, _)| name != "content-encoding")
        .collect();

    let incoming = response
        .consume()
        .map_err(|()| other("response body was already taken"))?;
    // A redirect's body is never used, so it is not read. Error bodies are
    // kept only for the message, so they get the smaller cap.
    let body = match status {
        300..=399 => Vec::new(),
        400..=599 => error_body(&incoming, &req.url, deadline, coding),
        _ => read_body(
            &incoming,
            &req.url,
            deadline,
            MAX_BODY_BYTES,
            body_coding(coding, &req.url)?,
        )?,
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

/// The coding to decode a body with, or an error naming the unsupported one.
fn body_coding(coding: Result<Coding, String>, target: &str) -> Result<Coding, TransportError> {
    coding.map_err(|c| {
        TransportError::Other(format!("unsupported Content-Encoding `{c}` from {target}"))
    })
}

/// The body of an error status, kept for the message. A body that cannot be
/// read or decoded is dropped rather than failing the exchange: the status is
/// what the caller classifies and retries on, and the body only adds detail.
fn error_body(
    body: &IncomingBody,
    target: &str,
    deadline: Option<u64>,
    coding: Result<Coding, String>,
) -> Vec<u8> {
    match body_coding(coding, target) {
        Ok(coding) => read_body(body, target, deadline, ERROR_BODY_CAP, coding).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// A blocking `Read` over a response body. Each chunk waits against the request
/// deadline, so a decoder reading through it is held to the same timeout as the
/// rest of the exchange.
struct BodyReader<'a> {
    stream: InputStream,
    target: &'a str,
    deadline: Option<u64>,
    /// The transport error behind a failed read. The decoder only sees an
    /// `io::Error`, so the cause is kept here and reported instead.
    failure: Option<TransportError>,
}

impl Read for BodyReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let want = (buf.len() as u64).min(READ_CHUNK);
        loop {
            let ready = {
                let pollable = self.stream.subscribe();
                wait_ready(&pollable, self.deadline)
            };
            if !ready {
                self.failure = Some(TransportError::Timeout(self.target.to_string()));
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "response body timed out",
                ));
            }
            match self.stream.read(want) {
                Ok(chunk) if chunk.is_empty() => continue,
                Ok(chunk) => {
                    buf[..chunk.len()].copy_from_slice(&chunk);
                    return Ok(chunk.len());
                }
                Err(StreamError::Closed) => return Ok(0),
                Err(StreamError::LastOperationFailed(e)) => {
                    self.failure = Some(TransportError::Other(format!(
                        "reading the response body: {e}"
                    )));
                    return Err(io::Error::other("reading the response body failed"));
                }
            }
        }
    }
}

/// Read and decode a response body, keeping at most `cap` decoded bytes and
/// waiting for each chunk against the request deadline. Oversized bodies are
/// truncated, not rejected, as on the native transport.
fn read_body(
    body: &IncomingBody,
    target: &str,
    deadline: Option<u64>,
    cap: usize,
    coding: Coding,
) -> Result<Vec<u8>, TransportError> {
    let stream: InputStream = body
        .stream()
        .map_err(|()| other("response body stream is unavailable"))?;
    let mut reader = BodyReader {
        stream,
        target,
        deadline,
        failure: None,
    };
    let result = content_encoding::read_capped(&mut reader, coding, cap);
    if let Some(failure) = reader.failure.take() {
        return Err(failure);
    }
    result.map_err(|e| TransportError::Other(format!("decoding the response body: {e}")))
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
