use bubblepaw_core::{CancellationToken, ModelRequest, Provider};
use bubblepaw_runner::{
    config::{EffectiveConfig, LoadOptions, ProviderSettings},
    provider::{ChatCompletionsProvider, ProviderRegistry, ScriptedProvider},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[test]
fn llama_server_chat_config_accepts_default_url_without_a_key() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("config.toml");
    std::fs::write(
        &file,
        r#"
[model]
main = "llama/local"
[providers.llama]
kind = "chat-completions"
base_url = "http://127.0.0.1:8080/v1"
reasoning = {}
"#,
    )
    .unwrap();
    let config = EffectiveConfig::load(LoadOptions {
        workspace: root.path().to_path_buf(),
        home: None,
        custom_file: Some(file),
        env: vec![],
        cli: vec![],
    })
    .unwrap();
    let settings = config.settings().unwrap();
    settings.validate_provider_setup().unwrap();
    assert!(settings.providers["llama"].key_env.is_none());
    let mut registry = ProviderRegistry::new("demo", Arc::new(ScriptedProvider::demo(false)));
    settings.register_providers(&mut registry).unwrap();
    assert!(registry.get("llama").is_some());

    let remote: ProviderSettings =
        toml::from_str("kind='chat-completions'\nbase_url='https://example.com/v1'").unwrap();
    assert!(remote.validate("remote").is_err());
}

#[tokio::test]
async fn keyless_chat_sends_no_authorization_to_local_fake() {
    use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
    let seen = Arc::new(AtomicBool::new(false));
    let app = Router::new().route("/v1/chat/completions", post(
        |State(seen): State<Arc<AtomicBool>>, headers: HeaderMap| async move {
            assert!(!headers.contains_key("authorization"));
            seen.store(true, Ordering::SeqCst);
            Json(serde_json::json!({"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]}))
        }
    )).with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let provider = ChatCompletionsProvider::without_key(base).unwrap();
    let response = provider
        .generate(
            ModelRequest {
                model: "local".into(),
                system_prompt: "Test".into(),
                ..Default::default()
            },
            Arc::new(|_| {}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.text, "ok");
    assert!(seen.load(Ordering::SeqCst));
    server.abort();
    assert!(ChatCompletionsProvider::without_key("https://example.com/v1").is_err());
}
