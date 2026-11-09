use super::*;
use bubblepaw_core::CancellationToken;
use bubblepaw_runner::{
    host_attach,
    host_transport::HostEndpoint,
    remote::{
        self, DeviceKind, Limits, RemoteConnect, RemotePair, RemoteReach, RemoteServeOptions,
        RemoteServer, SessionBackend,
    },
};
use std::path::Path;

async fn configured(root: &Path, program: &Path) -> anyhow::Result<HostContext> {
    let file = root.join("config.toml");
    std::fs::write(
        &file,
        format!(
            "[model]\nprovider='demo'\nmain='demo/first'\n[providers.demo]\nkind='scripted'\nmodels=[{{id='first'}}]\n[context]\nauto_summarize=false\n[agents]\nauto_wake=false\n[remote]\ncloudflared={}\n",
            serde_json::to_string(program)?
        ),
    )?;
    let config = HostConfig::load(ConfigOptions {
        workspace: Some(root.to_owned()),
        data_dir: Some(root.join("data")),
        config_file: Some(file),
        home: None,
        env: Some(Vec::new()),
        overrides: Vec::new(),
    })?;
    let store = config.open_store()?;
    HostContext::start(config.into_request(), store, HostLifetime::default(), None).await
}

#[tokio::test]
async fn remote_info_is_process_free_for_read_and_full_direct_and_attached_callers()
-> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let fake = fake::install(&root.path().join("bin"), &[], None);
    let log = root.path().join("versions.log");
    let sidecar = fake.program.with_file_name("fake-cloudflared.json");
    let mut script: serde_json::Value = serde_json::from_slice(&std::fs::read(&sidecar)?)?;
    script["version_log"] = json!(log);
    std::fs::write(sidecar, script.to_string())?;
    let host = configured(root.path(), &fake.program).await?;
    let data = root.path().join("data");
    let endpoint = HostEndpoint::bind(&data).await?;
    let stop = CancellationToken::new();
    let serving = tokio::spawn(host_attach::serve_embedded(
        endpoint,
        host.ctx().store.clone(),
        HostLifetime::default(),
        host.ctx().clone(),
        stop.clone(),
    ));
    for attached in [false, true] {
        let backend = if attached {
            SessionBackend::Attach(data.clone())
        } else {
            SessionBackend::Context(Box::new(host.ctx().clone()))
        };
        let options = RemoteServeOptions::in_data_dir(
            &data,
            RemoteReach::Local {
                port: 0,
                address: None,
            },
            "Inspection host".into(),
        );
        let server = RemoteServer::start(options, Some(backend))?;
        for profile in [Profile::Read, Profile::Full] {
            let device = root.path().join(format!("{attached}-{profile:?}"));
            let mut offer = server.pair(profile).await?;
            let address = server.address().unwrap();
            let pairing = remote::pair(RemotePair {
                address: address.clone(),
                host_id: server.host_id(),
                code: offer.code().into(),
                key_file: device.join("device.bpkey"),
                hosts: device.join("hosts.json"),
                device_name: "Inspector".into(),
                device_kind: DeviceKind::Other,
                limits: Limits::default(),
            });
            let (pending, request) = tokio::join!(pairing, offer.next());
            let (paired, approved) =
                tokio::join!(pending?.finish(), request.unwrap().approve(None));
            paired?;
            approved?;
            let client = remote::connect(RemoteConnect {
                address,
                host_id: server.host_id(),
                key_file: device.join("device.bpkey"),
                hosts: device.join("hosts.json"),
                limits: Limits::default(),
            })
            .await?;
            let result = client.call("remote.cloudflared.info", json!({})).await?;
            assert_eq!(result["checked"], false);
            assert!(result["info"].is_null());
            assert!(
                !log.exists(),
                "remote {profile:?}, attached={attached} launched a check"
            );
            client.close().await;
        }
        server.shutdown().await;
    }
    // A local check supplies cached data to remote callers. Changes invalidate
    // that data, without causing a remote caller to execute the replacement.
    let checked = host_api::dispatch(
        host.ctx(),
        Profile::Read,
        "remote.cloudflared.info",
        json!({}),
    )
    .await?;
    assert_eq!(checked["checked"], true);
    let mut remote_ctx = host.ctx().clone();
    remote_ctx.remote_caller = true;
    assert_eq!(
        host_api::dispatch(
            &remote_ctx,
            Profile::Read,
            "remote.cloudflared.info",
            json!({})
        )
        .await?["checked"],
        true
    );
    fake::invalidate(&fake.program);
    assert_eq!(
        host_api::dispatch(
            &remote_ctx,
            Profile::Full,
            "remote.cloudflared.info",
            json!({})
        )
        .await?["checked"],
        false
    );
    assert_eq!(std::fs::read_to_string(log)?.lines().count(), 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&fake.program)?.permissions();
        permissions.set_mode(permissions.mode() & !0o111);
        std::fs::set_permissions(&fake.program, permissions)?;
        let result = host_api::dispatch(
            &remote_ctx,
            Profile::Read,
            "remote.cloudflared.info",
            json!({}),
        )
        .await?;
        assert_eq!(result["checked"], false);
        assert!(result["info"].is_null());
    }
    stop.cancel();
    serving.await??;
    host.shutdown().await?;
    Ok(())
}
