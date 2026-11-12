use super::*;

#[tokio::test]
async fn full_remote_callers_cannot_select_a_host_executable_in_process_or_attached() -> Result<()>
{
    for attached in [false, true] {
        let mut fixture = fixture().await?;
        if attached {
            fixture.server.shutdown().await;
            let RemoteAddress::Relay(url) = &fixture.address else {
                unreachable!()
            };
            let data = fixture.root.path().join("data");
            let options = RemoteServeOptions::in_data_dir(
                &data,
                RemoteReach::Relay(url.clone()),
                "Test host".into(),
            );
            let mut server =
                RemoteServer::start(options, Some(remote::SessionBackend::Attach(data.clone())))?;
            server
                .open_control(&remote::RemotePaths::in_data_dir(&data).control)
                .await?;
            server.wait_online(Duration::from_secs(10)).await?;
            fixture.server = server;
        }
        let (connect, _) = fixture.pair("phone", Profile::Full).await?;
        let client = Client::connect_remote(connect).await?;
        let config = fixture.root.path().join("config.toml");
        let before = std::fs::read(&config)?;
        for (method, params) in [
            (
                "remote.cloudflared.set",
                json!({"path":"/must-never-be-inspected"}),
            ),
            ("remote.cloudflared.set", json!({"path":null})),
            ("remote.cloudflared.set", json!({"malformed":true})),
            (
                "config.set",
                json!({"key":"remote.cloudflared", "value":"/must-never-run"}),
            ),
            (
                "config.set",
                json!({"key":"remote", "value":{"cloudflared":"/must-never-run"}}),
            ),
        ] {
            assert!(
                matches!(
                    client.call(method, params).await,
                    Err(ClientError::Rpc { code: -32003, .. })
                ),
                "{method}, attached={attached}"
            );
        }
        assert!(matches!(
            client.set_cloudflared(None).await,
            Err(ClientError::Rpc { code: -32003, .. })
        ));
        assert_eq!(std::fs::read(config)?, before);
        let state = client.share_state().await?;
        assert_eq!(state.status, Some(remote::RemoteStatus::Online));
        assert!(state.last_error.is_none() && state.failure.is_none());
        client.close().await?;
        fixture.server.shutdown().await;
        fixture.host.close().await?;
    }
    Ok(())
}
