//! Explicit one-request live checks; never contacted by ordinary workspace tests.
use bubblepaw_runner::net::{FetchBackend, NetClient, SearchBackend, WebConfig};

#[tokio::test]
#[ignore = "Makes one public DuckDuckGo search; run only after approval"]
async fn live_ddg_search() {
    let client = NetClient::new(&WebConfig::default()).unwrap();
    let results = client
        .search("Rust programming language official", 3)
        .await
        .unwrap();
    assert!(!results.is_empty(), "search returned no results");
    println!("{}", serde_json::to_string_pretty(&results).unwrap());
}

#[tokio::test]
#[ignore = "Makes one SearXNG search; set BUBBLEPAW_LIVE_SEARXNG_URL and run only after approval"]
async fn live_searxng_search() {
    let config = WebConfig {
        search: vec![SearchBackend::SearxNg {
            url: std::env::var("BUBBLEPAW_LIVE_SEARXNG_URL")
                .expect("Set BUBBLEPAW_LIVE_SEARXNG_URL"),
        }],
        ..Default::default()
    };
    let results = NetClient::new(&config)
        .unwrap()
        .search("Rust programming language official", 3)
        .await
        .unwrap();
    assert!(!results.is_empty(), "search returned no results");
    println!("{}", serde_json::to_string_pretty(&results).unwrap());
}

#[tokio::test]
#[ignore = "Fetches one public page through a running Crawl4AI server; set BUBBLEPAW_LIVE_CRAWL4AI_URL and run only after approval"]
async fn live_crawl4ai_fetch() {
    let service = live_url("BUBBLEPAW_LIVE_CRAWL4AI_URL");
    let config = WebConfig {
        fetch: vec![FetchBackend::Crawl4Ai { url: service }],
        ..Default::default()
    };
    let page = NetClient::new(&config)
        .unwrap()
        .fetch("https://example.com/", 0, 2_000)
        .await
        .unwrap();
    assert!(
        !page.content.trim().is_empty(),
        "Crawl4AI returned no markdown"
    );
    println!("{}", serde_json::to_string_pretty(&page).unwrap());
}

#[tokio::test]
#[ignore = "Runs the SearXNG-then-DuckDuckGo search chain for one query; set BUBBLEPAW_LIVE_SEARXNG_URL and run only after approval"]
async fn live_search_fallback_chain() {
    let config = WebConfig {
        search: vec![
            SearchBackend::SearxNg {
                url: live_url("BUBBLEPAW_LIVE_SEARXNG_URL"),
            },
            SearchBackend::DuckDuckGo,
        ],
        ..Default::default()
    };
    let served = NetClient::new(&config)
        .unwrap()
        .search_served("Rust programming language official", 3)
        .await
        .unwrap();
    assert!(
        served.backend.starts_with("searxng:"),
        "served by {}",
        served.backend
    );
    assert!(!served.results.is_empty(), "search returned no results");
    println!("{}", serde_json::to_string_pretty(&served).unwrap());
}

#[tokio::test]
#[ignore = "Fetches https://example.com/ through the Crawl4AI-then-direct chain; set BUBBLEPAW_LIVE_CRAWL4AI_URL and CRAWL4AI_API_TOKEN and run only after approval"]
async fn live_fetch_fallback_chain() {
    let config = WebConfig {
        fetch: vec![
            FetchBackend::Crawl4Ai {
                url: live_url("BUBBLEPAW_LIVE_CRAWL4AI_URL"),
            },
            FetchBackend::Direct,
        ],
        ..Default::default()
    };
    let page = NetClient::new(&config)
        .unwrap()
        .fetch("https://example.com/", 0, 2_000)
        .await
        .unwrap();
    assert!(
        page.backend.starts_with("crawl4ai:"),
        "served by {}",
        page.backend
    );
    assert!(
        !page.content.trim().is_empty(),
        "Crawl4AI returned no markdown"
    );
    println!("{}", serde_json::to_string_pretty(&page).unwrap());
}

/// Live service URLs come only from the environment; there is no default host.
fn live_url(var: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| panic!("set {var} to run this live test"))
}
