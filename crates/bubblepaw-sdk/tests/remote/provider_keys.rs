use super::*;

#[tokio::test]
async fn paired_provider_keys_require_full_and_return_only_status() -> Result<()> {
    use bubblepaw_sdk::provider_keys::ProviderKeySource;
    let fixture = direct_fixture().await?;
    fixture
        .host
        .call(
            "providers.save",
            json!({
                "name":"key-fixture", "input": {
                    "kind":"chat-completions", "base_url":"https://example.invalid/v1",
                    "models":[{"id":"fixture"}]
                }
            }),
        )
        .await?;
    const KEY: &str = "synthetic-paired-provider-key-marker";
    for (name, profile) in [
        ("read-key", Profile::Read),
        ("edit-key", Profile::Edit),
        ("full-key", Profile::Full),
    ] {
        let (connect, _) = fixture.pair(name, profile).await?;
        let client = Client::connect_remote(connect).await?;
        let status = client.provider_key_status("key-fixture").await?;
        assert_eq!(status.source, ProviderKeySource::None);
        assert!(!status.is_set);
        if profile == Profile::Full {
            let saved = client.set_provider_key("key-fixture", KEY).await?;
            assert!(saved.applied);
            assert!(!saved.restart_needed);
            assert_eq!(saved.status.source, ProviderKeySource::Stored);
            assert!(saved.status.is_set);
            assert!(!format!("{saved:?}").contains(KEY));
            let local = fixture.host.provider_key_status("key-fixture").await?;
            assert_eq!(local, saved.status);
            let raw = client
                .call("providers.key.get", json!({"name":"key-fixture"}))
                .await?;
            assert_eq!(raw, json!({"source":"stored","is_set":true}));
            assert!(!raw.to_string().contains(KEY));
            let removed = client.remove_provider_key("key-fixture").await?;
            assert_eq!(removed.status.source, ProviderKeySource::None);
            assert!(!removed.status.is_set);
        } else {
            for error in [
                client
                    .set_provider_key("key-fixture", KEY)
                    .await
                    .unwrap_err(),
                client.remove_provider_key("key-fixture").await.unwrap_err(),
            ] {
                assert!(matches!(error, ClientError::Rpc { code: -32003, .. }));
                assert!(!format!("{error:?}").contains(KEY));
            }
            assert_eq!(
                fixture.host.provider_key_status("key-fixture").await?,
                status
            );
        }
        client.close().await?;
    }
    fixture.server.shutdown().await;
    fixture.host.close().await?;
    Ok(())
}
