//! Ordered backend fallback for web_search and web_fetch, against local HTTP mocks only.
#[path = "support/http_server.rs"]
mod http_server;
use axum::{
    Router,
    body::Body,
    http::Response,
    routing::{get, post},
};
use bubblepaw_runner::net::{FetchBackend, NetClient, SearchBackend, WebConfig};
use http_server::serve;
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

type Counter = Arc<AtomicUsize>;

fn counter() -> Counter {
    Arc::new(AtomicUsize::new(0))
}
fn hits(counter: &Counter) -> usize {
    counter.load(Ordering::SeqCst)
}
fn reply(status: u16, headers: &[(&str, &str)], body: String) -> Response<Body> {
    let mut builder = Response::builder().status(status);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::from(body)).unwrap()
}
const JSON: &[(&str, &str)] = &[("content-type", "application/json")];

/// An address with nothing listening, so connecting fails at once.
async fn closed() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    base
}

fn results(title: &str) -> String {
    json!({"results": [{
        "title": title,
        "url": "https://example.com/source",
        "content": format!("{title} snippet")
    }]})
    .to_string()
}

/// A SearXNG-shaped backend that counts requests and answers each with the same reply.
fn searx(
    hits: Counter,
    status: u16,
    headers: &'static [(&'static str, &'static str)],
    body: String,
) -> Router {
    Router::new().route(
        "/search",
        get(move || {
            let hits = hits.clone();
            let body = body.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                reply(status, headers, body)
            }
        }),
    )
}

fn searx_config(urls: &[&str]) -> WebConfig {
    WebConfig {
        search: urls
            .iter()
            .map(|url| SearchBackend::SearxNg {
                url: (*url).to_owned(),
            })
            .collect(),
        allow_private: true,
        ..Default::default()
    }
}

#[tokio::test]
async fn search_falls_back_on_http_error_and_unreachable_and_counts_one_budget_unit() {
    let (a, a_task) = serve(searx(counter(), 500, &[], String::new())).await;
    let dead = closed().await;
    let (c, c_task) = serve(searx(counter(), 200, JSON, results("C"))).await;
    let client = NetClient::new(&WebConfig {
        max_searches: Some(1),
        ..searx_config(&[&a, &dead, &c])
    })
    .unwrap();
    let served = client
        .search_served("fallback error chain", 8)
        .await
        .unwrap();
    assert_eq!(served.backend, format!("searxng:{c}"));
    assert_eq!(served.results[0].title, "C");
    // The three backends tried were one tool call, so the budget of one is spent.
    let error = client
        .search_served("fallback error chain again", 8)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("web search budget exhausted (limit 1)"),
        "{error}"
    );
    a_task.abort();
    c_task.abort();
}

#[tokio::test]
async fn search_falls_back_past_empty_results_and_keeps_an_empty_answer_when_nothing_better() {
    let (a, a_task) = serve(searx(
        counter(),
        200,
        JSON,
        json!({"results": []}).to_string(),
    ))
    .await;
    let (b, b_task) = serve(searx(counter(), 200, JSON, results("B"))).await;
    let client = NetClient::new(&searx_config(&[&a, &b])).unwrap();
    let served = client
        .search_served("fallback empty results", 8)
        .await
        .unwrap();
    assert_eq!(served.backend, format!("searxng:{b}"));
    assert_eq!(served.results[0].title, "B");

    // Only empty answers and failures: the empty answer stands, with no error.
    let dead = closed().await;
    let client = NetClient::new(&searx_config(&[&a, &dead])).unwrap();
    let served = client
        .search_served("fallback empty only", 8)
        .await
        .unwrap();
    assert_eq!(served.backend, format!("searxng:{a}"));
    assert!(served.results.is_empty());
    a_task.abort();
    b_task.abort();
}

#[tokio::test]
async fn search_falls_back_after_a_timeout() {
    let slow = Router::new().route(
        "/search",
        get(|| async {
            tokio::time::sleep(Duration::from_secs(25)).await;
            reply(200, JSON, results("slow"))
        }),
    );
    let (a, a_task) = serve(slow).await;
    let (b, b_task) = serve(searx(counter(), 200, JSON, results("B"))).await;
    let client = NetClient::new(&searx_config(&[&a, &b])).unwrap();
    let served = client
        .search_served("fallback timeout chain", 8)
        .await
        .unwrap();
    assert_eq!(served.backend, format!("searxng:{b}"));
    a_task.abort();
    b_task.abort();
}

#[tokio::test]
async fn rate_limit_cools_down_for_retry_after_then_the_backend_is_tried_again() {
    let hits_a = counter();
    let (a, a_task) = serve(searx(
        hits_a.clone(),
        429,
        &[("retry-after", "1")],
        String::new(),
    ))
    .await;
    let (b, b_task) = serve(searx(counter(), 200, JSON, results("B"))).await;
    let client = NetClient::new(&searx_config(&[&a, &b])).unwrap();

    let served = client
        .search_served("cooldown first call", 8)
        .await
        .unwrap();
    assert_eq!(served.backend, format!("searxng:{b}"));
    assert_eq!(hits(&hits_a), 1);

    // Skipped during the cooldown, so the limited backend is not contacted again.
    let served = client
        .search_served("cooldown second call", 8)
        .await
        .unwrap();
    assert_eq!(served.backend, format!("searxng:{b}"));
    assert_eq!(hits(&hits_a), 1);

    tokio::time::sleep(Duration::from_millis(1200)).await;
    // Expired: the backend is tried again, and is limited again.
    client
        .search_served("cooldown third call", 8)
        .await
        .unwrap();
    assert_eq!(hits(&hits_a), 2);
    a_task.abort();
    b_task.abort();
}

#[tokio::test]
async fn rate_limit_without_retry_after_uses_the_configured_cooldown_and_zero_disables_it() {
    let hits_a = counter();
    let (a, a_task) = serve(searx(hits_a.clone(), 429, &[], String::new())).await;
    let (b, b_task) = serve(searx(counter(), 200, JSON, results("B"))).await;
    let client = NetClient::new(&WebConfig {
        cooldown_secs: 60,
        ..searx_config(&[&a, &b])
    })
    .unwrap();
    client
        .search_served("default cooldown first", 8)
        .await
        .unwrap();
    client
        .search_served("default cooldown second", 8)
        .await
        .unwrap();
    assert_eq!(hits(&hits_a), 1);

    let hits_z = counter();
    let (z, z_task) = serve(searx(hits_z.clone(), 429, &[], String::new())).await;
    let client = NetClient::new(&WebConfig {
        cooldown_secs: 0,
        ..searx_config(&[&z, &b])
    })
    .unwrap();
    client.search_served("no cooldown first", 8).await.unwrap();
    client.search_served("no cooldown second", 8).await.unwrap();
    assert_eq!(hits(&hits_z), 2);
    a_task.abort();
    b_task.abort();
    z_task.abort();
}

#[tokio::test]
async fn brave_without_a_key_is_skipped_and_only_a_chain_of_brave_fails() {
    let (c, c_task) = serve(searx(counter(), 200, JSON, results("C"))).await;
    let client = NetClient::new(&WebConfig {
        search: vec![
            SearchBackend::Brave {
                api_key: String::new(),
            },
            SearchBackend::SearxNg { url: c.clone() },
        ],
        allow_private: true,
        ..Default::default()
    })
    .unwrap();
    let served = client
        .search_served("brave skipped chain", 8)
        .await
        .unwrap();
    assert_eq!(served.backend, format!("searxng:{c}"));

    let only_brave = NetClient::new(&WebConfig {
        search: vec![SearchBackend::Brave {
            api_key: String::new(),
        }],
        ..Default::default()
    })
    .unwrap();
    let error = only_brave
        .search("brave only", 8)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("brave: skipped, no API key"), "{error}");
    c_task.abort();
}

#[tokio::test]
async fn all_failing_search_backends_produce_one_error_listing_each() {
    let (a, a_task) = serve(searx(counter(), 500, &[], String::new())).await;
    let dead = closed().await;
    let client = NetClient::new(&searx_config(&[&a, &dead])).unwrap();
    let error = client
        .search_served("all fail chain", 8)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.starts_with("all web search backends failed: "),
        "{error}"
    );
    assert!(
        error.contains(&format!("searxng:{a}: web server returned HTTP 500")),
        "{error}"
    );
    assert!(error.contains(&format!("searxng:{dead}: ")), "{error}");
    a_task.abort();
}

fn html_page() -> Response<Body> {
    reply(
        200,
        &[("content-type", "text/html")],
        "<html><title>Direct</title><body><p>Direct body</p></body></html>".into(),
    )
}

#[tokio::test]
async fn fetch_falls_back_from_crawl4ai_errors_and_challenge_pages_and_counts_one_unit() {
    let failing = Router::new().route("/crawl", post(|| async { reply(500, &[], String::new()) }));
    let (x, x_task) = serve(failing).await;
    let challenge = json!({
        "success": true,
        "results": [{"success": true, "markdown": {"fit_markdown": "Just a moment... cf-challenge"}}]
    })
    .to_string();
    let challenged = Router::new().route(
        "/crawl",
        post(move || {
            let challenge = challenge.clone();
            async move { reply(200, JSON, challenge) }
        }),
    );
    let (y, y_task) = serve(challenged).await;
    let pages = Router::new()
        .route("/page", get(|| async { html_page() }))
        .route("/other", get(|| async { html_page() }));
    let (d, d_task) = serve(pages).await;
    let client = NetClient::new(&WebConfig {
        fetch: vec![
            FetchBackend::Crawl4Ai { url: x.clone() },
            FetchBackend::Crawl4Ai { url: y.clone() },
            FetchBackend::Direct,
        ],
        allow_private: true,
        max_fetches: Some(1),
        ..Default::default()
    })
    .unwrap();
    let fetched = client.fetch(&format!("{d}/page"), 0, 20_000).await.unwrap();
    assert_eq!(fetched.backend, "direct");
    assert_eq!(fetched.title, "Direct");
    assert!(fetched.content.contains("Direct body"));
    // The three backends were one fetch call, so the budget of one is spent.
    let error = client
        .fetch(&format!("{d}/other"), 0, 20_000)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("web fetch budget exhausted (limit 1)"),
        "{error}"
    );
    x_task.abort();
    y_task.abort();
    d_task.abort();
}

#[tokio::test]
async fn crawl4ai_rate_limit_cools_down_that_service_only() {
    let hits_x = counter();
    let counted = hits_x.clone();
    let limited = Router::new().route(
        "/crawl",
        post(move || {
            let counted = counted.clone();
            async move {
                counted.fetch_add(1, Ordering::SeqCst);
                reply(429, &[("retry-after", "60")], String::new())
            }
        }),
    );
    let (x, x_task) = serve(limited).await;
    let pages = Router::new()
        .route("/page", get(|| async { html_page() }))
        .route("/other", get(|| async { html_page() }));
    let (d, d_task) = serve(pages).await;
    let client = NetClient::new(&WebConfig {
        fetch: vec![
            FetchBackend::Crawl4Ai { url: x.clone() },
            FetchBackend::Direct,
        ],
        allow_private: true,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        client
            .fetch(&format!("{d}/page"), 0, 20_000)
            .await
            .unwrap()
            .backend,
        "direct"
    );
    assert_eq!(hits(&hits_x), 1);
    // The Crawl4AI service is skipped for the cooldown, not contacted again.
    assert_eq!(
        client
            .fetch(&format!("{d}/other"), 0, 20_000)
            .await
            .unwrap()
            .backend,
        "direct"
    );
    assert_eq!(hits(&hits_x), 1);
    x_task.abort();
    d_task.abort();
}

#[tokio::test]
async fn direct_rate_limits_from_a_target_site_do_not_cool_down_the_direct_backend() {
    let hits_target = counter();
    let counted = hits_target.clone();
    let target = Router::new().route(
        "/limited",
        get(move || {
            let counted = counted.clone();
            async move {
                counted.fetch_add(1, Ordering::SeqCst);
                reply(429, &[], String::new())
            }
        }),
    );
    let (t, t_task) = serve(target).await;
    let client = NetClient::new(&WebConfig {
        allow_private: true,
        ..Default::default()
    })
    .unwrap();
    for n in 1..=2 {
        let error = client
            .fetch(&format!("{t}/limited?n={n}"), 0, 20_000)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("HTTP 429"), "{error}");
    }
    // Both attempts reached the target: the direct backend was never skipped.
    assert_eq!(hits(&hits_target), 2);
    t_task.abort();
}
