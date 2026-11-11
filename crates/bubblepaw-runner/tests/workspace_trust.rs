//! Workspace trust: a workspace's `.bubblepaw/config.toml` applies only after the user
//! trusted the workspace. Scripted only: configuration loading and the `bubblepaw trust`
//! command in a temp sandbox; no provider is contacted.
use anyhow::{Result, ensure};
use bubblepaw_runner::{
    config::{EffectiveConfig, LoadOptions, Override},
    host_config::{self, ConfigOptions},
    workspace_trust::{self, TrustStatus},
};
use std::{
    io::Write,
    path::Path,
    process::Command,
    sync::{Arc, Mutex, OnceLock},
};

const RETENTION: &str = "storage.telemetry_retention_days";
const PROJECT: &str = "[storage]\ntelemetry_retention_days = 9\n";

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
impl Logs {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// One process-wide subscriber; each test finds its own lines by its unique temp path.
fn logs() -> &'static Logs {
    static LOGS: OnceLock<Logs> = OnceLock::new();
    LOGS.get_or_init(|| {
        let logs = Logs::default();
        let writer = logs.clone();
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .try_init();
        logs
    })
}

struct Fixture {
    _dir: tempfile::TempDir,
    opts: LoadOptions,
}
impl Fixture {
    fn new(project: &str) -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let (workspace, home) = (dir.path().join("workspace"), dir.path().join("home"));
        std::fs::create_dir_all(workspace.join(".bubblepaw"))?;
        std::fs::create_dir_all(home.join(".bubblepaw"))?;
        std::fs::write(workspace.join(".bubblepaw/config.toml"), project)?;
        let opts = LoadOptions {
            workspace,
            home: Some(home),
            custom_file: None,
            env: Vec::new(),
            cli: Vec::new(),
        };
        Ok(Self { _dir: dir, opts })
    }
    fn home(&self) -> &Path {
        self.opts.home.as_deref().unwrap()
    }
    fn load(&self) -> Result<EffectiveConfig> {
        EffectiveConfig::load(self.opts.clone())
    }
    fn retention(&self) -> Result<i64> {
        let config = self.load()?;
        let value = config.value(RETENTION).and_then(toml::Value::as_integer);
        Ok(value.expect("telemetry_retention_days is an integer"))
    }
}

#[test]
fn project_config_applies_only_while_the_workspace_is_trusted() -> Result<()> {
    let fixture = Fixture::new(PROJECT)?;
    assert_eq!(
        fixture.retention()?,
        30,
        "untrusted: the project layer is absent"
    );
    let state = fixture.load()?.workspace_trust();
    assert_eq!(state.status, TrustStatus::Untrusted);
    assert_eq!(state.sections, ["storage"]);
    assert!(state.how_to_trust.contains("bubblepaw trust"));

    workspace_trust::trust(fixture.home(), &fixture.opts.workspace)?;
    assert_eq!(fixture.retention()?, 9);
    assert_eq!(
        fixture.load()?.workspace_trust().status,
        TrustStatus::Trusted
    );

    assert!(workspace_trust::untrust(
        fixture.home(),
        &fixture.opts.workspace
    )?);
    assert_eq!(fixture.retention()?, 30);

    std::fs::remove_file(fixture.opts.workspace.join(".bubblepaw/config.toml"))?;
    let state = fixture.load()?.workspace_trust();
    assert_eq!(
        state.status,
        TrustStatus::NoProjectConfig,
        "no file, no decision"
    );
    Ok(())
}

#[test]
fn a_project_config_cannot_trust_itself() -> Result<()> {
    let fixture = Fixture::new("")?;
    let workspace = fixture.opts.workspace.to_string_lossy().replace('\\', "/");
    let file = fixture.opts.workspace.join(".bubblepaw/config.toml");
    std::fs::write(
        &file,
        format!(
            "[workspace_trust]\nenabled = false\ntrust_workspace = true\ntrusted = ['{workspace}']\n{PROJECT}"
        ),
    )?;
    assert_eq!(fixture.retention()?, 30);
    assert_eq!(
        fixture.load()?.workspace_trust().status,
        TrustStatus::Untrusted
    );

    // Trusted, the file still cannot rewrite the gate for itself.
    workspace_trust::trust(fixture.home(), &fixture.opts.workspace)?;
    let config = fixture.load()?;
    assert_eq!(fixture.retention()?, 9);
    let enabled = config.value("workspace_trust.enabled");
    assert_eq!(enabled.and_then(toml::Value::as_bool), Some(true));
    let trusted = config.value("workspace_trust.trusted");
    assert!(trusted.and_then(toml::Value::as_array).unwrap().is_empty());
    Ok(())
}

#[test]
fn headless_runs_skip_with_a_warning_and_each_grant_applies_the_config() -> Result<()> {
    let logs = logs();
    let mut fixture = Fixture::new(PROJECT)?;
    assert_eq!(fixture.retention()?, 30);
    let file = fixture.opts.workspace.join(".bubblepaw/config.toml");
    let warned = logs
        .text()
        .lines()
        .any(|line| line.contains(&file.display().to_string()) && line.contains("bubblepaw trust"));
    assert!(
        warned,
        "no warning naming the file and the fix:\n{}",
        logs.text()
    );

    let workspace = fixture.opts.workspace.to_string_lossy().replace('\\', "/");
    let user_file = fixture.home().join(".bubblepaw/config.toml");
    let base = fixture.opts.clone();
    type Grant = Box<dyn Fn(&mut LoadOptions) -> Result<()>>;
    let grants: [(&str, Grant); 5] = [
        (
            "user key trusted",
            Box::new({
                let (user_file, workspace) = (user_file.clone(), workspace.clone());
                move |_: &mut LoadOptions| {
                    Ok(std::fs::write(
                        &user_file,
                        format!("[workspace_trust]\ntrusted=['{workspace}']\n"),
                    )?)
                }
            }),
        ),
        (
            "user key enabled=false",
            Box::new({
                let user_file = user_file.clone();
                move |_: &mut LoadOptions| {
                    Ok(std::fs::write(
                        &user_file,
                        "[workspace_trust]\nenabled=false\n",
                    )?)
                }
            }),
        ),
        (
            "environment",
            Box::new(|opts: &mut LoadOptions| {
                opts.env.push((
                    "BUBBLEPAW_WORKSPACE_TRUST__TRUST_WORKSPACE".into(),
                    "true".into(),
                ));
                Ok(())
            }),
        ),
        (
            "CLI override",
            Box::new(|opts: &mut LoadOptions| {
                opts.cli = vec![Override::set("workspace_trust.trust_workspace=true")?];
                Ok(())
            }),
        ),
        (
            "SDK option",
            Box::new({
                let home = base.home.clone();
                move |opts: &mut LoadOptions| {
                    let options = ConfigOptions {
                        workspace: Some(opts.workspace.clone()),
                        home: home.clone(),
                        env: Some(Vec::new()),
                        ..ConfigOptions::default()
                    }
                    .trust_workspace();
                    let config = host_config::layer(&options)
                        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
                    assert_eq!(
                        config.value(RETENTION).and_then(toml::Value::as_integer),
                        Some(9)
                    );
                    Ok(())
                }
            }),
        ),
    ];
    for (name, grant) in grants {
        let _ = std::fs::remove_file(&user_file);
        fixture.opts = base.clone();
        grant(&mut fixture.opts)?;
        if name != "SDK option" {
            assert_eq!(fixture.retention()?, 9, "{name}");
        }
    }
    Ok(())
}

fn bubblepaw(home: &Path, workspace: &Path, args: &[&str]) -> Result<String> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bubblepaw"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("BUBBLEPAW_") {
            command.env_remove(key);
        }
    }
    let output = command
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("LOCALAPPDATA", home.join("local"))
        .env("APPDATA", home.join("roaming"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("CODEX_HOME", home.join("codex"))
        .current_dir(workspace)
        .args(args)
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

fn retention_in(config: &str) -> Result<i64> {
    let config: toml::Value = toml::from_str(config)?;
    Ok(config["storage"]["telemetry_retention_days"]
        .as_integer()
        .unwrap())
}

#[test]
fn trust_command_is_remembered_by_later_processes() -> Result<()> {
    let fixture = Fixture::new(PROJECT)?;
    let (home, workspace) = (fixture.home(), fixture.opts.workspace.as_path());
    assert_eq!(retention_in(&bubblepaw(home, workspace, &["config"])?)?, 30);
    assert_eq!(
        retention_in(&bubblepaw(
            home,
            workspace,
            &["--trust-workspace", "config"]
        )?)?,
        9,
        "the flag trusts one run"
    );
    assert_eq!(retention_in(&bubblepaw(home, workspace, &["config"])?)?, 30);

    bubblepaw(home, workspace, &["trust"])?;
    assert_eq!(retention_in(&bubblepaw(home, workspace, &["config"])?)?, 9);
    let listed = bubblepaw(home, workspace, &["trust", "--list"])?;
    assert!(listed.contains("workspace"), "{listed}");

    bubblepaw(home, workspace, &["trust", "--revoke"])?;
    assert_eq!(retention_in(&bubblepaw(home, workspace, &["config"])?)?, 30);
    Ok(())
}
