//! Fetch through an in-memory `Transport`: no sockets, no guard. Covers the
//! redirect loop and the transient-retry classification that `fetch_with`
//! shares with the native `fetch_page`.

use std::collections::VecDeque;
use std::sync::Mutex;

use webfetch::fetch_with;
use webfetch_core::transport::{HttpRequest, HttpResponse, Transport, TransportError};

/// Replays canned replies in order and records every request it was sent.
struct Fake {
    replies: Mutex<VecDeque<Result<HttpResponse, TransportError>>>,
    seen: Mutex<Vec<HttpRequest>>,
}

impl Fake {
    fn new(replies: Vec<Result<HttpResponse, TransportError>>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            seen: Mutex::new(Vec::new()),
        }
    }

    fn seen(&self) -> Vec<HttpRequest> {
        self.seen.lock().unwrap().clone()
    }
}

impl Transport for Fake {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, TransportError> {
        self.seen.lock().unwrap().push(req);
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("fetch sent more requests than the test scripted")
    }

    /// Retries do not wait in tests: the fake answers the next scripted reply
    /// at once.
    async fn sleep(&self, _d: std::time::Duration) {}
}

fn reply(
    status: u16,
    headers: &[(&str, &str)],
    body: &str,
) -> Result<HttpResponse, TransportError> {
    Ok(HttpResponse {
        status,
        headers: headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        body: body.as_bytes().to_vec(),
    })
}

const PAGE: &str = "<html><body><article><p>hello there</p></article></body></html>";

#[tokio::test]
async fn redirect_is_followed_with_a_second_send() {
    let fake = Fake::new(vec![
        reply(302, &[("location", "/final")], ""),
        reply(200, &[("content-type", "text/html")], PAGE),
    ]);

    let page = fetch_with(&fake, "https://example.test/start", 5)
        .await
        .expect("the redirect should be followed");

    assert_eq!(page.final_url, "https://example.test/final");
    assert!(page.body.contains("hello there"), "{}", page.body);

    let seen = fake.seen();
    assert_eq!(seen.len(), 2, "one send per hop");
    assert_eq!(seen[0].url, "https://example.test/start");
    assert_eq!(seen[1].url, "https://example.test/final");
}

#[tokio::test]
async fn transient_transport_error_is_retried() {
    let fake = Fake::new(vec![
        Err(TransportError::Connect("connection refused".into())),
        reply(200, &[("content-type", "text/html")], PAGE),
    ]);

    let page = fetch_with(&fake, "https://example.test/", 5)
        .await
        .expect("a connect failure is transient and should be retried");

    assert!(page.body.contains("hello there"));
    assert_eq!(fake.seen().len(), 2);
}

#[tokio::test]
async fn transient_status_is_retried() {
    let fake = Fake::new(vec![
        reply(503, &[], "busy"),
        reply(200, &[("content-type", "text/html")], PAGE),
    ]);

    fetch_with(&fake, "https://example.test/", 5)
        .await
        .expect("a 5xx is transient");
    assert_eq!(fake.seen().len(), 2);
}

#[tokio::test]
async fn tls_failure_is_not_retried() {
    let fake = Fake::new(vec![Err(TransportError::Tls("unknown issuer".into()))]);

    let err = fetch_with(&fake, "https://example.test/", 5)
        .await
        .expect_err("a TLS failure is final");

    assert!(format!("{err:#}").contains("unknown issuer"), "{err:#}");
    assert_eq!(fake.seen().len(), 1, "a TLS error is not retried");
}
