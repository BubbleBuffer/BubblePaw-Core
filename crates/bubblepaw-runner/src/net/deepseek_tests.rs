use super::*;
use axum::{
    Router,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};

type RequestLog = Arc<Mutex<Vec<(Option<String>, Value)>>>;

async fn fixture() -> (String, RequestLog, tokio::task::JoinHandle<()>) {
    let log = RequestLog::default();
    let recorded = log.clone();
    let redirected = log.clone();
    let app = Router::new()
        .route("/v1/tools/search", post(move |headers: header::HeaderMap, axum::Json(body): axum::Json<Value>| {
            let recorded = recorded.clone();
            async move {
                let authorization = headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok()).map(str::to_owned);
                let query = body["query"].as_str().unwrap_or_default().to_owned();
                recorded.lock().unwrap().push((authorization, body));
                if query == "limited" {
                    return (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "600")], "synthetic-private-key").into_response();
                }
                if query == "balance" {
                    return (StatusCode::PAYMENT_REQUIRED, axum::Json(json!({"error":{"code":"InsufficientBalance","message":"synthetic-private-key"}}))).into_response();
                }
                if query == "redirect" {
                    return axum::response::Redirect::temporary("/redirect-target").into_response();
                }
                if query == "error" {
                    return (StatusCode::UNAUTHORIZED, "synthetic-private-key").into_response();
                }
                if query == "malformed" {
                    return axum::Json(json!({"results":"synthetic-private-key"})).into_response();
                }
                axum::Json(json!({"results":[
                    {"title":"DeepSeek fixture", "url":"https://example.org/deepseek", "snippet":"Supporting snippet", "date":"2026-10-18"},
                    {"title":"Undated", "url":"https://example.org/undated", "snippet":null, "date":null},
                    {"title":"Rejected", "url":"file:///private", "snippet":"bad", "date":null}
                ]})).into_response()
            }
        }))
        .route("/redirect-target", get(move |headers: header::HeaderMap| {
            let redirected = redirected.clone();
            async move {
                redirected.lock().unwrap().push((headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok()).map(str::to_owned), json!({"redirect_target":true})));
                (StatusCode::OK, "synthetic-private-key")
            }
        }))
        .route("/search", get(|| async { axum::Json(json!({"results":[{"title":"Fallback", "url":"https://example.org/fallback", "content":"Fallback snippet"}]})) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (base, log, server)
}

fn backend(base: &str, key: &str) -> SearchBackend {
    SearchBackend::DeepSeek {
        base_url: format!("{base}/v1/tools"),
        api_key: key.into(),
    }
}

#[tokio::test]
async fn post_contract_normalization_cache_and_key_partition() {
    let (base, log, server) = fixture().await;
    let config = WebConfig {
        search: vec![backend(&base, "synthetic-private-key")],
        ..Default::default()
    };
    let client = NetClient::new(&config).unwrap();
    let reply = client.search_served("  Public   Query ", 1).await.unwrap();
    assert_eq!(reply.backend, "deepseek");
    assert_eq!(reply.results.len(), 1);
    assert_eq!(reply.results[0].title, "DeepSeek fixture");
    assert_eq!(reply.results[0].url, "https://example.org/deepseek");
    assert_eq!(reply.results[0].snippet, "Supporting snippet");
    assert_eq!(reply.results[0].published_at.as_deref(), Some("2026-10-18"));
    let cached = client.search("public query", 20).await.unwrap();
    assert_eq!(cached.len(), 2);
    assert_eq!(cached[1].snippet, "");
    assert_eq!(cached[1].published_at, None);
    assert_eq!(
        log.lock().unwrap().as_slice(),
        &[(
            Some("Bearer synthetic-private-key".into()),
            json!({"query":"public query","count":20,"need_content":false})
        )]
    );
    let rotated = WebConfig {
        search: vec![backend(&base, "rotated-private-key")],
        ..config
    };
    NetClient::new(&rotated)
        .unwrap()
        .search("public query", 1)
        .await
        .unwrap();
    assert_eq!(log.lock().unwrap().len(), 2);
    let identity = search_identity(&rotated.search[0]);
    assert!(!identity.contains("rotated-private-key"));
    assert!(!format!("{rotated:?}").contains("rotated-private-key"));
    server.abort();
}

#[tokio::test]
async fn rate_limit_cooldown_missing_key_and_fallback() {
    let (base, log, server) = fixture().await;
    let fallback = SearchBackend::SearxNg { url: base.clone() };
    let config = WebConfig {
        search: vec![backend(&base, "synthetic-private-key"), fallback.clone()],
        ..Default::default()
    };
    let client = NetClient::new(&config).unwrap();
    assert!(
        client
            .search_served("balance", 1)
            .await
            .unwrap()
            .backend
            .starts_with("searxng:")
    );
    assert!(
        client
            .search_served("limited", 1)
            .await
            .unwrap()
            .backend
            .starts_with("searxng:")
    );
    assert!(
        client
            .search_served("another query", 1)
            .await
            .unwrap()
            .backend
            .starts_with("searxng:")
    );
    assert_eq!(
        log.lock().unwrap().len(),
        2,
        "Retry-After suppresses the next DeepSeek request"
    );
    let missing = WebConfig {
        search: vec![backend(&base, ""), fallback],
        ..Default::default()
    };
    assert!(
        NetClient::new(&missing)
            .unwrap()
            .search_served("missing key query", 1)
            .await
            .unwrap()
            .backend
            .starts_with("searxng:")
    );
    assert_eq!(log.lock().unwrap().len(), 2);
    server.abort();
}

#[tokio::test]
async fn failures_and_redirects_do_not_expose_key_or_response_body() {
    let (base, log, server) = fixture().await;
    let config = WebConfig {
        search: vec![backend(&base, "synthetic-private-key")],
        cache_ttl_secs: 0,
        ..Default::default()
    };
    let client = NetClient::new(&config).unwrap();
    for query in ["error", "redirect", "malformed", "balance"] {
        let error = client.search(query, 1).await.unwrap_err().to_string();
        assert!(!error.contains("synthetic-private-key"));
        assert!(error.contains("deepseek"));
    }
    assert_eq!(log.lock().unwrap().len(), 4);
    let invalid = WebConfig {
        search: vec![backend(&base, "private\nkey")],
        ..Default::default()
    };
    assert_eq!(
        NetBackendSnapshot::from_config(Some(&invalid))
            .unwrap_err()
            .to_string(),
        "invalid DeepSeek API key header"
    );
    server.abort();
}

#[test]
fn defaults_choose_deepseek_then_duckduckgo() {
    assert!(matches!(
        SearchBackend::default(),
        SearchBackend::DeepSeek { .. }
    ));
    let config: WebConfig = serde_json::from_str("{}").unwrap();
    assert!(
        matches!(config.search.as_slice(), [SearchBackend::DeepSeek { base_url, .. }, SearchBackend::DuckDuckGo] if base_url == DEEPSEEK_SEARCH_BASE_URL)
    );
}
