//! Shared primitives for the webfetch/websearch tools: text compression and
//! token budgeting ([`compress`]), reference-style URL preservation and the
//! reference-aware token budget ([`refs`]), HTTP primitives — user agent,
//! body cap, retry classification ([`http`]) — and the [`transport`]
//! abstraction the libraries send requests through. The native
//! [`transport::ReqwestTransport`] and shared TLS trust configuration ([`tls`])
//! are behind the `native` feature; the WASI 0.2 [`transport::WasiTransport`]
//! is behind `wasi`. Both leaf crates re-export these so their internal modules
//! can keep using `crate::compress` / `crate::refs` / `crate::tls`.

pub mod charset;
pub mod compress;
pub mod http;
pub mod refs;
pub mod transport;

#[cfg(feature = "native")]
pub mod tls;

#[cfg(feature = "wasi")]
mod wasi_transport;
