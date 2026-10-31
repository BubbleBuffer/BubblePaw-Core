use super::*;

#[tokio::test]
async fn providers_list_and_main_role_share_redacted_live_configuration() {
    let root = fixture();
    let file = root.path().join("config.toml");
    let mut host = Host::start(root.path(), &[]);
    let initial = host.ok("providers.list", json!({})).await;
    assert_eq!(initial["main"], "demo/first");
    assert_eq!(initial["active_main"], "demo/first");
    let demo = initial["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|provider| provider["name"] == "demo")
        .unwrap();
    assert_eq!(demo["key_present"], true);
    assert!(!initial.to_string().contains(SECRET));
    for input in [
        json!({}),
        json!({"model":false}),
        json!({"model":"bare"}),
        json!({"model":"missing/model"}),
        json!({"model":"demo/has space"}),
        json!({"model":"demo/second","extra":true}),
    ] {
        let response = host.call("roles.set_main", input).await;
        assert!(response.get("error").is_some(), "{response}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), CONFIG);
        assert!(!response.to_string().contains(SECRET));
    }
    let applied = host
        .ok("roles.set_main", json!({"model":"demo/second"}))
        .await;
    assert_eq!(applied["main"], "demo/second");
    assert_eq!(applied["active_main"], "demo/second");
    assert_eq!(applied["reload_needed"], false);
    assert!(!applied.to_string().contains(SECRET));
    let saved: toml::Value = toml::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(saved["model"]["main"].as_str(), Some("demo/second"));
    let current = host.ok("providers.list", json!({})).await;
    assert_eq!(current["active_main"], "demo/second");
    assert!(
        host.ok("session.list", json!({}))
            .await
            .as_array()
            .unwrap()
            .is_empty()
    );
    host.stop().await;
    let mut reopened = Host::start(root.path(), &[]);
    assert_eq!(
        reopened.ok("providers.list", json!({})).await["active_main"],
        "demo/second"
    );
    reopened.stop().await;
}

#[tokio::test]
async fn main_role_rejects_a_later_configuration_override_without_writing() {
    let root = fixture();
    let mut host = Host::start(root.path(), &["--model", "demo/first"]);
    let response = host
        .call("roles.set_main", json!({"model":"demo/second"}))
        .await;
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("overrides")
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("config.toml")).unwrap(),
        CONFIG
    );
    assert_eq!(
        host.ok("providers.list", json!({})).await["active_main"],
        "demo/first"
    );
    host.stop().await;
}
