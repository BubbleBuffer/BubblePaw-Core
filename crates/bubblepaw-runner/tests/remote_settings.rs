//! Local-only executable selection persists to the ordinary host config.
#[allow(dead_code)]
#[path = "remote_tunnel/fake.rs"]
mod fake;

use bubblepaw_runner::{
    host_api::{self, Profile},
    host_config::{ConfigOptions, HostConfig},
    host_lifetime::HostLifetime,
    session_host::host_context::HostContext,
};
use serde_json::json;

#[path = "remote_settings/inspection.rs"]
mod inspection;

#[tokio::test]
async fn a_validated_choice_is_persisted_without_copying_and_can_be_cleared() -> anyhow::Result<()>
{
    let root = tempfile::tempdir()?;
    let config_path = root.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[model]\nprovider='demo'\nmain='demo/first'\n[providers.demo]\nkind='scripted'\nmodels=[{id='first'}]\n[context]\nauto_summarize=false\n[agents]\nauto_wake=false\n",
    )?;
    let config = HostConfig::load(ConfigOptions {
        workspace: Some(root.path().to_owned()),
        data_dir: Some(root.path().join("data")),
        config_file: Some(config_path.clone()),
        home: None,
        env: Some(Vec::new()),
        overrides: Vec::new(),
    })?;
    let store = config.open_store()?;
    let host =
        HostContext::start(config.into_request(), store, HostLifetime::default(), None).await?;
    let fake = fake::install(&root.path().join("selected-bin"), &[], None);
    let sidecar = fake.program.with_file_name("fake-cloudflared.json");
    std::fs::write(&sidecar, json!({"version":"2026.9.0"}).to_string())?;
    let before = std::fs::read(&config_path)?;
    assert!(
        host_api::dispatch(
            host.ctx(),
            Profile::Full,
            "remote.cloudflared.set",
            json!({"path":root.path().join("missing")})
        )
        .await
        .is_err()
    );
    assert_eq!(std::fs::read(&config_path)?, before);
    let chosen = host_api::dispatch(
        host.ctx(),
        Profile::Full,
        "remote.cloudflared.set",
        json!({"path":fake.program}),
    )
    .await?;
    assert_eq!(chosen["info"]["chosen"], true);
    assert_eq!(chosen["info"]["managed"], false);
    assert!(!root.path().join("data/remote/cloudflare/bin").exists());
    let saved = HostConfig::load(ConfigOptions {
        workspace: Some(root.path().to_owned()),
        data_dir: Some(root.path().join("data")),
        config_file: Some(config_path.clone()),
        home: None,
        env: Some(Vec::new()),
        overrides: Vec::new(),
    })?;
    assert_eq!(
        std::path::Path::new(saved.settings().remote.cloudflared.as_deref().unwrap())
            .canonicalize()?,
        fake.program.canonicalize()?
    );
    let info = host_api::dispatch(
        host.ctx(),
        Profile::Read,
        "remote.cloudflared.info",
        json!({}),
    )
    .await?;
    assert_eq!(info["info"]["chosen"], true);
    host_api::dispatch(
        host.ctx(),
        Profile::Full,
        "remote.cloudflared.set",
        json!({"path":null}),
    )
    .await?;
    assert!(!std::fs::read_to_string(config_path)?.contains("cloudflared"));
    assert!(fake.program.exists());
    host.shutdown().await?;
    Ok(())
}
