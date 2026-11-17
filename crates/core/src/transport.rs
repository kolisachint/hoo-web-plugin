//! The HTTP seam the libraries send requests through.
//!
//! Everything above this module builds an [`HttpRequest`], hands it to a
//! [`Transport`], and reads an [`HttpResponse`] back. The native implementation
//! is [`ReqwestTransport`] (behind the `native` feature). A host that cannot use
//! reqwest, such as a WASI component on `wasi:http`, implements [`Transport`]
//! itself and calls the same generic entry points.
//!
//! One `send` is one hop: a transport never follows redirects. The 3xx response
//! is returned as-is so the caller can re-validate the target before issuing the
//! next request. Bodies are capped at [`crate::http::MAX_BODY_BYTES`].

use std::fmt;
use std::future::Future;
use std::time::Duration;

#[cfg(feature = "native")]
use crate::http::{push_capped, MAX_BODY_BYTES, USER_AGENT};
#[cfg(feature = "native")]
use crate::tls::TlsConfig;

/// HTTP method. The libraries only ever need these two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

/// One request, owned so it can be retried or re-sent after a redirect.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    /// Upper bound for this one request, covering connect, headers and body.
    /// `None` leaves the transport's own default in place.
    pub timeout: Option<Duration>,
}

/// One response. The body is already read and capped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    /// Header names are lowercase.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// First value of a header, matched case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Why a request produced no response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    /// The request did not finish within its timeout.
    Timeout,
    /// The connection could not be made or the request could not be sent.
    Connect(String),
    /// The TLS handshake or certificate check failed.
    Tls(String),
    /// The body exceeded the cap and the transport refuses to truncate it.
    /// `ReqwestTransport` truncates instead and never returns this.
    BodyTooLarge,
    /// Anything else, with the transport's own description.
    Other(String),
}

impl TransportError {
    /// Is the failure worth retrying? Timeouts and connection-level failures
    /// are; TLS, oversize and other errors are final. This is the classification
    /// the native path has always used.
    pub fn is_transient(&self) -> bool {
        matches!(self, TransportError::Timeout | TransportError::Connect(_))
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::Timeout => write!(f, "request timed out"),
            TransportError::Connect(m) => write!(f, "{m}"),
            TransportError::Tls(m) => write!(f, "TLS error: {m}"),
            TransportError::BodyTooLarge => write!(f, "response body exceeds the size cap"),
            TransportError::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for TransportError {}

/// Sends one HTTP request and returns its response, without following redirects.
pub trait Transport: Send + Sync {
    fn send(
        &self,
        req: HttpRequest,
    ) -> impl Future<Output = Result<HttpResponse, TransportError>> + Send;
}

/// Lets a borrowed transport be used wherever an owned one is expected.
impl<T: Transport + ?Sized> Transport for &T {
    fn send(
        &self,
        req: HttpRequest,
    ) -> impl Future<Output = Result<HttpResponse, TransportError>> + Send {
        (**self).send(req)
    }
}

/// The native transport: reqwest with the shared user agent, TLS trust from
/// [`TlsConfig`], gzip and brotli decoding, and redirects disabled.
///
/// The client is built once per instance. Callers that pin a host to specific
/// addresses (the fetch path's DNS-rebinding guard) build one instance per hop
/// with [`ReqwestTransport::pinned`].
#[cfg(feature = "native")]
#[derive(Debug, Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

#[cfg(feature = "native")]
impl ReqwestTransport {
    /// A transport with the shared client options and the given TLS trust.
    pub fn new(tls: &TlsConfig) -> Result<Self, TransportError> {
        Self::build(tls, None)
    }

    /// As [`ReqwestTransport::new`], but `host` resolves only to `addrs`. An
    /// empty `addrs` (the guard is disabled) leaves resolution alone.
    pub fn pinned(
        tls: &TlsConfig,
        host: &str,
        addrs: &[std::net::SocketAddr],
    ) -> Result<Self, TransportError> {
        Self::build(tls, Some((host, addrs)).filter(|(_, a)| !a.is_empty()))
    }

    fn build(
        tls: &TlsConfig,
        pin: Option<(&str, &[std::net::SocketAddr])>,
    ) -> Result<Self, TransportError> {
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(USER_AGENT)
            .gzip(true)
            .brotli(true);
        builder = tls
            .apply(builder)
            .map_err(|e| TransportError::Other(format!("{e:#}")))?;
        if let Some((host, addrs)) = pin {
            builder = builder.resolve_to_addrs(host, addrs);
        }
        let client = builder
            .build()
            .map_err(|e| TransportError::Other(format!("building HTTP client: {e}")))?;
        Ok(Self { client })
    }
}

#[cfg(feature = "native")]
fn map_send_error(e: reqwest::Error) -> TransportError {
    if e.is_timeout() {
        TransportError::Timeout
    } else if e.is_connect() || e.is_request() {
        TransportError::Connect(e.to_string())
    } else {
        TransportError::Other(e.to_string())
    }
}

/// Read a body under the cap. Oversized bodies are truncated, not rejected,
/// which is the behaviour the fetch and search paths have always had.
#[cfg(feature = "native")]
async fn read_capped(mut resp: reqwest::Response) -> Result<Vec<u8>, TransportError> {
    let mut buf: Vec<u8> = Vec::new();
    // Honour Content-Length to pre-size, but never trust it past the cap.
    if let Some(len) = resp.content_length() {
        buf.reserve(len.min(MAX_BODY_BYTES as u64) as usize);
    }
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                if push_capped(&mut buf, &chunk, MAX_BODY_BYTES) {
                    break;
                }
            }
            Ok(None) => break,
            Err(e) if e.is_timeout() => return Err(TransportError::Timeout),
            Err(e) => return Err(TransportError::Other(e.to_string())),
        }
    }
    Ok(buf)
}

#[cfg(feature = "native")]
impl Transport for ReqwestTransport {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, TransportError> {
        let mut builder = match req.method {
            Method::Get => self.client.get(&req.url),
            Method::Post => self.client.post(&req.url),
        };
        for (name, value) in &req.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        if let Some(body) = req.body {
            builder = builder.body(body);
        }
        if let Some(timeout) = req.timeout {
            builder = builder.timeout(timeout);
        }

        let resp = builder.send().await.map_err(map_send_error)?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
            .collect();
        let body = read_capped(resp).await?;
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_lookup_is_case_insensitive() {
        let resp = HttpResponse {
            status: 302,
            headers: vec![("location".into(), "/next".into())],
            body: Vec::new(),
        };
        assert_eq!(resp.header("Location"), Some("/next"));
        assert_eq!(resp.header("content-type"), None);
    }

    #[test]
    fn only_timeouts_and_connection_failures_are_transient() {
        assert!(TransportError::Timeout.is_transient());
        assert!(TransportError::Connect("refused".into()).is_transient());
        assert!(!TransportError::Tls("unknown issuer".into()).is_transient());
        assert!(!TransportError::BodyTooLarge.is_transient());
        assert!(!TransportError::Other("bad header".into()).is_transient());
    }

    #[cfg(feature = "native")]
    #[test]
    fn native_transport_builds_from_default_tls() {
        assert!(ReqwestTransport::new(&TlsConfig::default()).is_ok());
    }
}
