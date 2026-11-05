//! Workspace configuration cannot select the host's tunnel executable.
//! No executable is inspected or run: these tests exercise configuration only.
use anyhow::Result;
use bubblepaw_runner::config::{EffectiveConfig, LoadOptions, Override};
use std::{
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
};

const UNTRUSTED: &str = "workspace-executable-private-marker";
const USER: &str = "user-cloudflared-choice";

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);
impl Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn options(root: &Path) -> LoadOptions {
    LoadOptions {
        workspace: root.join("workspace"),
        home: Some(root.join("home")),
        custom_file: None,
        env: Vec::new(),
        cli: Vec::new(),
    }
}

fn selected(config: &EffectiveConfig) -> Result<Option<String>> {
    Ok(config.settings()?.remote.cloudflared)
}

#[test]
fn workspace_cloudflared_is_ignored_but_explicit_host_configuration_is_accepted() -> Result<()> {
    let logs = Logs::default();
    let writer = logs.clone();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .init();
    let root = tempfile::tempdir()?;
    let opts = options(root.path());
    let project_file = opts.workspace.join(".bubblepaw/config.toml");
    let user_file = opts.home.as_ref().unwrap().join(".bubblepaw/config.toml");
    std::fs::create_dir_all(project_file.parent().unwrap())?;
    std::fs::create_dir_all(user_file.parent().unwrap())?;
    let project_text = format!(
        "[remote]\ncloudflared='{UNTRUSTED}'\nenabled=true\nmode='domain'\nhostname='paw.example.com'\nport=9876\nrelay='wss://relay.example.com'\n"
    );
    std::fs::write(&project_file, &project_text)?;

    let project_only = EffectiveConfig::load(opts.clone())?;
    assert_eq!(selected(&project_only)?, None);
    assert!(project_only.source("remote.cloudflared").is_none());
    // Other workspace remote settings keep their existing layering behavior.
    let remote = project_only.settings()?.remote;
    assert!(remote.enabled);
    assert_eq!(remote.mode, bubblepaw_runner::config::RemoteMode::Domain);
    assert_eq!(remote.port, 9876);
    assert_eq!(remote.hostname.as_deref(), Some("paw.example.com"));
    assert_eq!(remote.relay.as_deref(), Some("wss://relay.example.com"));
    assert_eq!(std::fs::read_to_string(&project_file)?, project_text);

    std::fs::write(&user_file, format!("[remote]\ncloudflared='{USER}'\n"))?;
    let trusted_user = EffectiveConfig::load(opts.clone())?;
    assert_eq!(selected(&trusted_user)?.as_deref(), Some(USER));
    assert!(
        trusted_user
            .source("remote.cloudflared")
            .unwrap()
            .starts_with("user: ")
    );
    assert_eq!(selected(&trusted_user.reload()?)?.as_deref(), Some(USER));

    // Replacement candidates (used by settings writes) retain the file layer's trust.
    let replacement = format!("[remote]\ncloudflared={{ value='{UNTRUSTED}' }}\nport=0\n");
    let candidate = EffectiveConfig::load_with_file(opts.clone(), &project_file, &replacement)?;
    assert_eq!(selected(&candidate)?.as_deref(), Some(USER));
    assert_eq!(candidate.settings()?.remote.port, 0);

    let mut environment = opts.clone();
    environment.env.push((
        "BUBBLEPAW_REMOTE__CLOUDFLARED".into(),
        "environment-cloudflared-choice".into(),
    ));
    let from_env = EffectiveConfig::load(environment)?;
    assert_eq!(
        selected(&from_env)?.as_deref(),
        Some("environment-cloudflared-choice")
    );
    assert_eq!(from_env.source("remote.cloudflared"), Some("env"));
    let mut cli = opts.clone();
    cli.cli.push(Override::set(
        "remote.cloudflared='cli-cloudflared-choice'",
    )?);
    let from_cli = EffectiveConfig::load(cli)?;
    assert_eq!(
        selected(&from_cli)?.as_deref(),
        Some("cli-cloudflared-choice")
    );
    assert_eq!(from_cli.source("remote.cloudflared"), Some("CLI"));

    // Choosing a file explicitly is the user's trust decision, even when the
    // chosen file happens to live in the workspace.
    let warnings_before_explicit = logs.0.lock().unwrap().len();
    let mut explicit = opts.clone();
    explicit.custom_file = Some(project_file.clone());
    let from_custom = EffectiveConfig::load(explicit)?;
    assert_eq!(selected(&from_custom)?.as_deref(), Some(UNTRUSTED));
    assert!(
        from_custom
            .source("remote.cloudflared")
            .unwrap()
            .starts_with("custom: ")
    );
    let mut selected_file = opts.clone();
    selected_file.env.push((
        "BUBBLEPAW_CONFIG".into(),
        project_file.to_string_lossy().into_owned(),
    ));
    assert_eq!(
        selected(&EffectiveConfig::load(selected_file)?)?.as_deref(),
        Some(UNTRUSTED)
    );
    assert_eq!(logs.0.lock().unwrap().len(), warnings_before_explicit);

    // A saved host choice uses the trusted user file; workspace precedence
    // cannot replace or invalidate it, including after its value is cleared.
    let updated = EffectiveConfig::load_with_file(
        opts.clone(),
        &user_file,
        "[remote]\ncloudflared='updated-host-choice'\n",
    )?;
    assert_eq!(selected(&updated)?.as_deref(), Some("updated-host-choice"));
    let cleared = EffectiveConfig::load_with_file(opts.clone(), &user_file, "[remote]\n")?;
    assert_eq!(selected(&cleared)?, None);
    let invalid_workspace = EffectiveConfig::load_with_file(
        opts,
        &project_file,
        "[remote]\nunknown_workspace_key=true\n",
    );
    assert!(invalid_workspace.is_err());

    let warnings = String::from_utf8(logs.0.lock().unwrap().clone())?;
    assert!(warnings.contains("Ignoring remote.cloudflared in workspace .bubblepaw/config.toml"));
    assert!(warnings.contains("user configuration"));
    assert!(warnings.contains("BUBBLEPAW_CONFIG/--config"));
    assert!(warnings.contains("remote.cloudflared.set"));
    assert!(!warnings.contains(UNTRUSTED));
    Ok(())
}
