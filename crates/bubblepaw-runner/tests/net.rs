use axum::{
    Router,
    body::Body,
    http::{Response, StatusCode},
    response::Redirect,
    routing::get,
};
use bubblepaw_core::{CancellationToken, ToolContext, ToolRegistry};
use bubblepaw_runner::net::{
    NetClient, SearchBackend, WebConfig, register_fetch_search_tools, register_research_sources,
};
use serde_json::json;

async fn fixture() -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new()
        .route("/html", get(|| async { Response::builder().header("content-type","text/html").body(Body::from("<html><title>Fixture &amp; title</title><style>badstyle</style><script>badscript</script><nav>badnav</nav><h1>Hello</h1><p>Readable <strong>page</strong> with <a href='https://example.com'>a link</a>.</p></html>")).unwrap() }))
        .route("/text", get(|| async { Response::builder().header("content-type","text/plain").body(Body::from("a🐾bcé" )).unwrap() }))
        .route("/unicode", get(|| async { Response::builder().header("content-type","text/plain").body(Body::from("🐾".repeat(50_000))).unwrap() }))
        .route("/json", get(|| async { Response::builder().header("content-type","application/json").body(Body::from("{\"ok\":true}")).unwrap() }))
        .route("/binary", get(|| async { Response::builder().header("content-type","image/png").body(Body::from("fake")).unwrap() }))
        .route("/large", get(|| async { Response::builder().header("content-type","text/plain").body(Body::from(vec![b'a';5_000_001])).unwrap() }))
        .route("/at-limit", get(|| async { Response::builder().header("content-type","text/plain").body(Body::from(vec![b'a';5_000_000])).unwrap() }))
        .route("/stream-large", get(|| async {
            let chunks = futures_util::stream::iter((0..6).map(|_| Ok::<_,std::io::Error>(vec![b'a';1024*1024])));
            Response::builder().header("content-type","text/plain").body(Body::from_stream(chunks)).unwrap()
        }))
        .route("/redirect", get(|| async { Redirect::temporary("/html") }))
        .route("/loop", get(|| async { Redirect::temporary("/loop") }))
        .route("/search", get(|| async { axum::Json(json!({"results":[{"title":"One","url":"https://example.com/source","content":"Supporting snippet"}]})) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, task)
}
fn local_config() -> WebConfig {
    WebConfig {
        allow_private: true,
        ..Default::default()
    }
}

#[tokio::test]
async fn html_title_markdown_and_unicode_paging() {
    let (url, server) = fixture().await;
    let client = NetClient::new(&local_config()).unwrap();
    let html = client
        .fetch(&format!("{url}/html"), 0, 20_000)
        .await
        .unwrap();
    assert_eq!(html.title, "Fixture & title");
    assert!(html.content.contains("Hello") && html.content.contains("Readable"));
    assert!(
        !html.content.contains("badscript")
            && !html.content.contains("badstyle")
            && !html.content.contains("badnav")
    );
    assert!(html.content.contains("https://example.com"));
    let page = client.fetch(&format!("{url}/text"), 1, 2).await.unwrap();
    assert_eq!(page.content, "🐾b");
    assert_eq!(page.total_chars, 5);
    assert_eq!(page.next_offset, Some(3));
    let page = client
        .fetch(&format!("{url}/text"), 3, 20_000)
        .await
        .unwrap();
    assert_eq!(page.content, "cé");
    assert_eq!(page.next_offset, None);
    assert_eq!(
        client
            .fetch(&format!("{url}/json"), 0, 20_000)
            .await
            .unwrap()
            .content,
        "{\"ok\":true}"
    );
    assert!(client.fetch(&format!("{url}/text"), 6, 1).await.is_err());
    let unicode = client
        .fetch(&format!("{url}/unicode"), 0, 100_000)
        .await
        .unwrap();
    assert!(serde_json::to_vec(&unicode).unwrap().len() <= 60 * 1024);
    assert_eq!(unicode.next_offset, Some(unicode.content.chars().count()));
    assert_eq!(unicode.total_chars, 50_000);
    server.abort();
}

#[tokio::test]
async fn caps_types_and_manual_redirects() {
    let (url, server) = fixture().await;
    let client = NetClient::new(&local_config()).unwrap();
    let boundary = client
        .fetch(&format!("{url}/at-limit"), 0, 1)
        .await
        .unwrap();
    assert_eq!(boundary.total_chars, 5_000_000);
    assert_eq!(boundary.content, "a");
    assert_eq!(boundary.next_offset, Some(1));
    for endpoint in ["large", "stream-large"] {
        assert!(
            client
                .fetch(&format!("{url}/{endpoint}"), 0, 20_000)
                .await
                .unwrap_err()
                .to_string()
                .contains("5 MB")
        );
    }
    assert!(
        client
            .fetch(&format!("{url}/binary"), 0, 20_000)
            .await
            .unwrap_err()
            .to_string()
            .contains("content type")
    );
    assert!(
        client
            .fetch("file:///etc/passwd", 0, 20_000)
            .await
            .unwrap_err()
            .to_string()
            .contains("http or https")
    );
    assert!(
        client
            .fetch(&format!("{url}/redirect"), 0, 20_000)
            .await
            .unwrap()
            .url
            .ends_with("/html")
    );
    assert!(
        client
            .fetch(&format!("{url}/loop"), 0, 20_000)
            .await
            .unwrap_err()
            .to_string()
            .contains("redirect limit")
    );
    server.abort();
}

#[tokio::test]
async fn address_policy_and_searx_exemption_does_not_cover_fetch_or_redirects() {
    let (url, server) = fixture().await;
    let config = WebConfig {
        search: vec![SearchBackend::SearxNg { url: url.clone() }],
        ..Default::default()
    };
    let client = NetClient::new(&config).unwrap();
    assert_eq!(
        client.search("fixture", 8).await.unwrap()[0].snippet,
        "Supporting snippet"
    );
    for url in [
        format!("{url}/html"),
        "http://10.0.0.1/".into(),
        "http://169.254.169.254/latest/meta-data".into(),
        "http://[::1]/".into(),
        "http://[fd00::1]/".into(),
    ] {
        assert!(
            client
                .fetch(&url, 0, 20_000)
                .await
                .unwrap_err()
                .to_string()
                .contains("prohibited address")
        );
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let redirect_url = format!("http://{}", listener.local_addr().unwrap());
    let target = format!("{url}/html");
    let redirect_server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/search",
                get(move || {
                    let target = target.clone();
                    async move { (StatusCode::FOUND, [("location", target)]) }
                }),
            ),
        )
        .await
        .unwrap();
    });
    let client = NetClient::new(&WebConfig {
        search: vec![SearchBackend::SearxNg { url: redirect_url }],
        ..Default::default()
    })
    .unwrap();
    // Backend identity and policy partition cached search results.
    assert!(
        client
            .search("fixture redirect policy", 8)
            .await
            .unwrap_err()
            .to_string()
            .contains("prohibited address")
    );
    redirect_server.abort();
    server.abort();
}

#[tokio::test]
async fn registrations_are_read_only_host_tools_and_cancel_requests() {
    let mut registry = ToolRegistry::new();
    register_fetch_search_tools(&mut registry, &WebConfig::default()).unwrap();
    assert!(registry.get_host("web_fetch").is_some());
    assert!(registry.get_host("web_search").is_some());
    assert!(registry.get("web_search").unwrap().definition().read_only);
    assert!(registry.definitions().is_empty());
    assert!(registry.is_shown_always("web_fetch"));
    let mut child = ToolRegistry::new();
    register_research_sources(&mut child, &WebConfig::default()).unwrap();
    assert!(child.get_host("web_search").unwrap().definition().read_only);
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let context = ToolContext {
        session_id: "s".into(),
        run_id: "r".into(),
        invocation_id: "i".into(),
        cancellation,
        approval: Default::default(),
    };
    assert!(
        child
            .get_host("web_fetch")
            .unwrap()
            .invoke(context, json!({"url":"https://example.com"}))
            .await
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
}
