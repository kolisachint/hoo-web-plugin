//! Search through an in-memory `Transport`: canned provider JSON in, parsed
//! results out, with the request the provider built checked on the way.

use std::collections::VecDeque;
use std::sync::Mutex;

use webfetch_core::transport::{HttpRequest, HttpResponse, Transport, TransportError};
use websearch::providers::{brave, searxng};
use websearch::types::SearchOptions;

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
            .expect("search sent more requests than the test scripted")
    }

    /// Retries do not wait in tests: the fake answers the next scripted reply
    /// at once.
    async fn sleep(&self, _d: std::time::Duration) {}
}

fn json_ok(body: &str) -> Result<HttpResponse, TransportError> {
    Ok(HttpResponse {
        status: 200,
        headers: vec![("content-type".into(), "application/json".into())],
        body: body.as_bytes().to_vec(),
    })
}

fn options(query: &str) -> SearchOptions {
    SearchOptions {
        query: query.into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn brave_parses_canned_response_and_sends_key_in_header() {
    let body = r#"{"type":"search","web":{"results":[
        {"title":"Rust async book","url":"https://rust-lang.github.io/async-book/",
         "description":"Asynchronous programming in <strong>Rust</strong>."},
        {"title":"Tokio","url":"https://tokio.rs","description":"An async runtime."}]}}"#;
    let fake = Fake::new(vec![json_ok(body)]);

    let (results, status) = brave::search_with(&fake, &options("rust async"), 5, "KEY-123")
        .await
        .expect("brave search");

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].url, "https://rust-lang.github.io/async-book/");
    assert_eq!(results[1].snippet, "An async runtime.");
    assert_eq!(status, websearch::types::SearchStatus::Ok);

    let seen = fake.seen();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].url.starts_with("https://api.search.brave.com/"));
    assert!(seen[0].url.contains("q=rust+async"), "{}", seen[0].url);
    assert!(
        seen[0]
            .headers
            .iter()
            .any(|(k, v)| k == "X-Subscription-Token" && v == "KEY-123"),
        "key must travel in the header"
    );
    assert!(
        !seen[0].url.contains("KEY-123"),
        "key must not be in the URL"
    );
}

#[tokio::test]
async fn searxng_parses_canned_response() {
    let body = r#"{"query":"x","results":[
        {"url":"https://a.test/1","title":"Alpha","content":"About alpha."},
        {"url":"https://b.test/2","title":"Bravo","content":"About bravo."}]}"#;
    let fake = Fake::new(vec![json_ok(body)]);

    let (results, status) = searxng::search_with(
        &fake,
        &options("alpha"),
        5,
        "https://searx.test/",
        Some("TOKEN"),
    )
    .await
    .expect("searxng search");

    assert_eq!(status, websearch::types::SearchStatus::Ok);
    assert_eq!(results.len(), 2);
    assert_eq!(results[1].title, "Bravo");

    let seen = fake.seen();
    assert_eq!(
        seen[0].url.split('?').next(),
        Some("https://searx.test/search")
    );
    assert!(seen[0]
        .headers
        .iter()
        .any(|(k, v)| k == "Authorization" && v == "Bearer TOKEN"));
}

#[tokio::test]
async fn transient_failure_is_retried_by_duckduckgo() {
    let page = r#"<html><body><table>
        <tr><td><a class="result-link" href="https://a.test/">Alpha</a></td></tr>
        <tr><td class="result-snippet">About alpha.</td></tr></table></body></html>"#;
    let fake = Fake::new(vec![
        Err(TransportError::Connect("connection reset".into())),
        Ok(HttpResponse {
            status: 200,
            headers: vec![],
            body: page.as_bytes().to_vec(),
        }),
    ]);

    let (_, _) = websearch::providers::ddg::search_with(&fake, &options("alpha"), 5)
        .await
        .expect("a connect failure is retried");
    assert_eq!(fake.seen().len(), 2);
}
