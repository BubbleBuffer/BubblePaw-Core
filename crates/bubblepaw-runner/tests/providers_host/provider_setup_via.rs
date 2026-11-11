#![cfg(feature = "web")]

//! The provider-setup editor does not edit `providers.<name>.via`, so saving a
//! provider keeps the `via` its file already holds. Ephemeral files and a
//! loopback server only; nothing is contacted.
use bubblepaw_runner::{
    config::{EffectiveConfig, LoadOptions},
    web_providers::{ProviderSetup, router},
};
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn saving_a_provider_keeps_its_via() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let workspace = temp.path().join("workspace");
    let custom = temp.path().join("custom.toml");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    let device = bubblepaw_remote::device::DeviceKey::generate()
        .unwrap()
        .id()
        .to_string();
    std::fs::write(
        &custom,
        format!(
            "[providers.local]\nkind='chat-completions'\nbase_url='https://api.example.com/v1'\nkey_env='BP_WEB_MISSING_CREDENTIAL_75462_KEY'\nvia='{device}'\n"
        ),
    )
    .unwrap();
    let options = LoadOptions {
        workspace,
        home: Some(home),
        custom_file: Some(custom.clone()),
        env: Vec::new(),
        cli: Vec::new(),
    };
    let launched = EffectiveConfig::load(options.clone()).unwrap();
    let app = router(ProviderSetup::new(options.clone(), &launched).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap()
        .put(format!("{url}/api/providers/local"))
        .json(&json!({
            "kind": "chat-completions",
            "base_url": "https://api.example.com/v1",
            "key_env": "BP_WEB_MISSING_CREDENTIAL_75462_KEY",
            "models": [{"id": "model-two"}]
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    task.abort();
    assert!(status.is_success(), "{status}: {body}");
    let doc: toml::Value = toml::from_str(&std::fs::read_to_string(&custom).unwrap()).unwrap();
    assert_eq!(doc["providers"]["local"]["via"].as_str(), Some(&*device));
    assert_eq!(
        doc["providers"]["local"]["models"][0]["id"].as_str(),
        Some("model-two")
    );
    let loaded = EffectiveConfig::load(options).unwrap().settings().unwrap();
    assert_eq!(loaded.providers["local"].via.as_deref(), Some(&*device));
}
