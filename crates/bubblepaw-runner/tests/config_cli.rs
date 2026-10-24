//! Layered configuration through the real `bubblepaw` binary: `config`, `--config`,
//! `--set`, legacy short flags and `--extensions`. Scripted only: no provider is
//! called, and HOME, the workspace and data directories live in a temp sandbox.
use anyhow::{Context, Result, ensure};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use toml::Value;

const SECRET: &str = "sk-LOCAL-FIXTURE-SECRET-7f3a";

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        fs::create_dir_all(dir.path().join("workspace"))?;
        fs::write(dir.path().join("empty.toml"), "")?;
        Ok(Self { dir })
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn workspace(&self) -> PathBuf {
        self.path().join("workspace")
    }

    fn write(&self, rel: &str, body: &str) -> Result<PathBuf> {
        let path = self.path().join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, body)?;
        Ok(path)
    }

    /// Runs the binary without `--config`. HOME does not exist in the sandbox, so
    /// the user file is missing, and no credentials are present.
    fn run(&self, argv: &[&str], env: &[(&str, &str)]) -> Result<Output> {
        let home = self.path().join("home");
        let mut command = Command::new(env!("CARGO_BIN_EXE_bubblepaw"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("BUBBLEPAW_") {
                command.env_remove(key);
            }
        }
        command
            .env_remove("OPENAI_API_KEY")
            .env_remove("OPENROUTER_API_KEY")
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("LOCALAPPDATA", home.join("local"))
            .env("APPDATA", home.join("roaming"))
            .env("XDG_DATA_HOME", home.join("data"))
            .env("CODEX_HOME", home.join("codex"))
            .current_dir(self.workspace())
            .args(argv)
            .envs(env.iter().copied());
        Ok(command.output()?)
    }

    /// Runs `argv`, requires success and parses stdout as TOML.
    fn show(&self, argv: &[&str]) -> Result<Value> {
        let output = self.run(argv, &[])?;
        ensure!(output.status.success(), "{}", failure_text(&output));
        Ok(String::from_utf8(output.stdout)?.parse()?)
    }

    /// Like `show`, but with an explicit empty file so no user layer applies.
    fn effective(&self, argv: &[&str]) -> Result<Value> {
        let empty = self.path().join("empty.toml");
        let empty = empty.to_str().context("sandbox path is not UTF-8")?;
        let mut full = vec!["--config", empty];
        full.extend_from_slice(argv);
        self.show(&full)
    }
}

fn failure_text(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn at<'a>(value: &'a Value, path: &[&str]) -> Result<&'a Value> {
    let mut current = value;
    for key in path {
        current = current
            .get(*key)
            .with_context(|| format!("missing key {}", path.join(".")))?;
    }
    Ok(current)
}

fn scalar(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}

fn snapshot(root: &Path) -> Result<Vec<String>> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = path.strip_prefix(root)?.display().to_string();
            if entry.file_type()?.is_dir() {
                out.push(format!("{name}/"));
                walk(&path, root, out)?;
            } else {
                out.push(format!("{name} {}", fs::metadata(&path)?.len()));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(root, root, &mut out)?;
    out.sort();
    Ok(out)
}

fn assert_key_error(output: &Output, sentinel: &str) {
    let text = failure_text(output);
    assert!(!output.status.success(), "{text}");
    let lower = text.to_lowercase();
    assert!(lower.contains("model") && lower.contains("nope"), "{text}");
    assert!(!text.contains(sentinel), "{text}");
}

#[test]
fn effective_config_is_parsable_with_sources_and_no_secret_values() -> Result<()> {
    let sb = Sandbox::new()?;
    let output = sb.run(
        &[
            "--set",
            "providers.opencode.key_env=\"BUBBLEPAW_TEST_ROLE_KEY\"",
            "config",
        ],
        &[
            ("BUBBLEPAW_TEST_ROLE_KEY", SECRET),
            ("OPENAI_API_KEY", SECRET),
            ("BUBBLEPAW_CONTEXT__COMPACT_THRESHOLD_TOKENS", "64000"),
        ],
    )?;
    ensure!(output.status.success(), "{}", failure_text(&output));
    let text = failure_text(&output);
    assert!(!text.contains(SECRET), "{text}");

    let stdout = String::from_utf8(output.stdout.clone())?;
    let value: Value = stdout.parse()?;
    assert_eq!(
        scalar(at(&value, &["providers", "opencode", "key_env"])?),
        "BUBBLEPAW_TEST_ROLE_KEY"
    );
    assert_eq!(
        scalar(at(&value, &["context", "compact_threshold_tokens"])?),
        "64000"
    );

    let comments: Vec<&str> = stdout
        .lines()
        .filter(|line| line.trim_start().starts_with('#'))
        .collect();
    ensure!(!comments.is_empty(), "no source comments:\n{stdout}");
    let lower = comments.join("\n").to_lowercase();
    assert!(
        lower.contains("default") || lower.contains("built-in"),
        "{lower}"
    );
    assert!(lower.contains("cli") || lower.contains("--set"), "{lower}");
    assert!(lower.contains("env"), "{lower}");
    Ok(())
}

#[test]
fn prompt_file_reaches_saved_history_and_root_round_limit_reaches_harness() -> Result<()> {
    let sb = Sandbox::new()?;
    let task = "Inspect \"quoted paths\".\nKeep $literal and `code` intact. 猫\n";
    let prompt_file = sb.write("worker task.txt", task)?;
    let prompt_file = prompt_file.to_str().context("task path is not UTF-8")?;
    let empty = sb.path().join("empty.toml");
    let empty = empty.to_str().context("config path is not UTF-8")?;
    let data = sb.path().join("worker-data");
    let data = data.to_str().context("data path is not UTF-8")?;
    let prefix = [
        "--config",
        empty,
        "--provider",
        "demo",
        "--read-only",
        "--no-auto-summarize",
        "--data-dir",
        data,
        "--json",
    ];
    let mut argv = prefix.to_vec();
    argv.extend([
        "--set",
        "agent.max_rounds=2",
        "run",
        "--prompt-file",
        prompt_file,
    ]);
    let output = sb.run(&argv, &[])?;
    ensure!(output.status.success(), "{}", failure_text(&output));
    let records: Vec<serde_json::Value> = String::from_utf8(output.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    let result = records
        .iter()
        .find(|record| record["type"] == "result")
        .context("missing worker result")?;
    assert_eq!(result["run"]["status"], "completed");
    let session = result["session_id"]
        .as_str()
        .context("missing session id")?;
    let mut argv = prefix.to_vec();
    argv.extend(["history", session]);
    let output = sb.run(&argv, &[])?;
    ensure!(output.status.success(), "{}", failure_text(&output));
    let history: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)?;
    assert!(history.iter().any(|message| message["role"] == "user"
        && message["origin"] == "typed"
        && message["content"] == task));

    let mut argv = prefix.to_vec();
    argv.extend(["--set", "agent.max_rounds=1", "run", "same demo, one round"]);
    let limited = sb.run(&argv, &[])?;
    assert!(!limited.status.success());
    assert!(failure_text(&limited).contains("Maximum model/tool rounds reached"));
    Ok(())
}

#[test]
fn unreadable_prompt_file_fails_before_creating_worker_storage() -> Result<()> {
    let sb = Sandbox::new()?;
    let prompt_file = sb.path().join("missing.txt");
    let prompt_file = prompt_file.to_str().context("task path is not UTF-8")?;
    let data = sb.path().join("unused-data");
    let output = sb.run(
        &[
            "--provider",
            "demo",
            "--data-dir",
            data.to_str().unwrap(),
            "run",
            "--prompt-file",
            prompt_file,
        ],
        &[],
    )?;
    assert!(!output.status.success());
    assert!(failure_text(&output).contains("read UTF-8 prompt file"));
    assert!(!data.exists(), "bad task input must not initialize storage");
    Ok(())
}

#[test]
fn short_flags_equal_their_set_keys() -> Result<()> {
    let sb = Sandbox::new()?;
    let defaults = sb.effective(&["config"])?;
    assert_eq!(scalar(at(&defaults, &["model", "main"])?), "gpt-6-luna");
    assert_eq!(scalar(at(&defaults, &["model", "provider"])?), "codex");

    // Each flag must yield the same effective config as the --set form it expands to.
    // Role lists are compared whole, including the default repeats and threshold.
    let cases: &[(&[&str], &[&str])] = &[
        (
            &["--model", "demo-model"],
            &["--set", "model.main=\"demo-model\""],
        ),
        (
            &["--provider", "demo"],
            &["--set", "model.provider=\"demo\""],
        ),
        (
            &["--context-strategy", "classical"],
            &["--set", "context.strategy=\"classical\""],
        ),
        (
            &["--compact-threshold-tokens", "128000"],
            &["--set", "context.compact_threshold_tokens=128000"],
        ),
        (
            &["--no-auto-summarize"],
            &["--set", "context.auto_summarize=false"],
        ),
        (
            &["--approve-all"],
            &["--set", "approvals.confirm=\"allow\""],
        ),
        (
            &["--confirm", "write,list"],
            &[
                "--set",
                "approvals.tools.write=\"ask\"",
                "--set",
                "approvals.tools.list=\"ask\"",
            ],
        ),
        (
            &["--confirm-auto", "write,list"],
            &[
                "--set",
                "approvals.tools.write=\"auto\"",
                "--set",
                "approvals.tools.list=\"auto\"",
            ],
        ),
        (
            &["--summary-model", "opencode/mock"],
            &["--set", "roles.summary=[\"opencode/mock\",\"main\"]"],
        ),
        (
            &["--decider", "llm/demo/check"],
            &[
                "--set",
                "roles.decider=[\"llm/demo/check\",\"llm/summary\",\"llm/main\"]",
            ],
        ),
        (
            &["--approver", "llm/demo/check"],
            &[
                "--set",
                "roles.approver=[\"llm/demo/check\",\"llm/summary\",\"llm/main\"]",
            ],
        ),
        (
            &["--auto-approve"],
            &["--set", "approvals.confirm=\"auto\""],
        ),
        (
            &["--auto-approve=llm/demo/check"],
            &[
                "--set",
                "approvals.confirm=\"auto\"",
                "--set",
                "roles.approver=[\"llm/demo/check\",\"llm/summary\",\"llm/main\"]",
            ],
        ),
    ];
    for (flags, set) in cases {
        let mut flag_argv = flags.to_vec();
        flag_argv.push("config");
        let mut set_argv = set.to_vec();
        set_argv.push("config");
        assert_eq!(
            sb.effective(&flag_argv)?,
            sb.effective(&set_argv)?,
            "{flags:?} differs from {set:?}"
        );
    }

    Ok(())
}

#[test]
fn later_argument_wins_in_every_order() -> Result<()> {
    let sb = Sandbox::new()?;
    // A = alias flag, S = --set pair, C = the config subcommand.
    let orders = [
        ("ASC", "set"),
        ("SAC", "alias"),
        ("CAS", "set"),
        ("CSA", "alias"),
        ("ACS", "set"),
        ("SCA", "alias"),
    ];
    type AliasPair<'a> = (
        &'a [&'a str],
        &'a [&'a str],
        &'a [&'a str],
        &'a str,
        &'a str,
    );
    let pairs: [AliasPair<'_>; 3] = [
        (
            &["--model", "alias-model"],
            &["--set", "model.main=\"set-model\""],
            &["model", "main"],
            "alias-model",
            "set-model",
        ),
        (
            &["--context-strategy", "classical"],
            &["--set", "context.strategy=\"lcm\""],
            &["context", "strategy"],
            "classical",
            "lcm",
        ),
        (
            &["--compact-threshold-tokens", "222222"],
            &["--set", "context.compact_threshold_tokens=111111"],
            &["context", "compact_threshold_tokens"],
            "222222",
            "111111",
        ),
    ];
    for (alias, set, path, alias_value, set_value) in pairs {
        for (order, winner) in orders {
            let mut argv = Vec::new();
            for token in order.chars() {
                match token {
                    'A' => argv.extend_from_slice(alias),
                    'S' => argv.extend_from_slice(set),
                    _ => argv.push("config"),
                }
            }
            let value = sb.effective(&argv)?;
            let expected = if winner == "set" {
                set_value
            } else {
                alias_value
            };
            assert_eq!(
                scalar(at(&value, path)?),
                expected,
                "{path:?} with order {order} (argv {argv:?})"
            );
        }
    }
    Ok(())
}

#[test]
fn extensions_json_matches_toml_table_and_resolves_cwd_from_file_parent() -> Result<()> {
    let sb = Sandbox::new()?;
    let json = sb.write(
        "cfg/ext.json",
        r#"[{"id":"demo","command":"demo-host","args":["--serve"],"cwd":"tools"}]"#,
    )?;
    let toml_file = sb.write(
        "cfg/config.toml",
        "[extensions.demo]\ncommand = \"demo-host\"\nargs = [\"--serve\"]\ncwd = \"tools\"\n",
    )?;
    let json = json.to_str().context("json path is not UTF-8")?;
    let toml_file = toml_file.to_str().context("toml path is not UTF-8")?;

    let from_json = sb.show(&["--extensions", json, "config"])?;
    let from_toml = sb.show(&["--config", toml_file, "config"])?;
    let json_table = at(&from_json, &["extensions", "demo"])?;
    assert_eq!(json_table, at(&from_toml, &["extensions", "demo"])?);

    assert_eq!(scalar(at(json_table, &["command"])?), "demo-host");
    assert_eq!(
        at(json_table, &["args"])?,
        &Value::Array(vec![Value::String("--serve".into())])
    );
    // The relative cwd belongs to the file's parent (cfg/), not the workspace.
    let cwd = scalar(at(json_table, &["cwd"])?);
    assert!(
        Path::new(&cwd).ends_with(Path::new("cfg").join("tools")),
        "cwd resolved to {cwd}"
    );
    Ok(())
}

#[test]
fn bad_config_errors_name_the_key_without_printing_values() -> Result<()> {
    let sb = Sandbox::new()?;
    let missing = sb.path().join("missing.toml");
    let missing = missing.to_str().context("path is not UTF-8")?;
    let output = sb.run(&["--config", missing, "config"], &[])?;
    let text = failure_text(&output);
    assert!(!output.status.success(), "{text}");
    assert!(text.contains("missing.toml"), "{text}");

    let output = sb.run(
        &["--set", "model.nope=\"CLI-SENTINEL-VALUE\"", "config"],
        &[],
    )?;
    assert_key_error(&output, "CLI-SENTINEL-VALUE");

    let output = sb.run(
        &["config"],
        &[("BUBBLEPAW_MODEL__NOPE", "ENV-SENTINEL-VALUE")],
    )?;
    assert_key_error(&output, "ENV-SENTINEL-VALUE");

    let file = sb.write("bad.toml", "[model]\nnope = \"FILE-SENTINEL-VALUE\"\n")?;
    let file = file.to_str().context("path is not UTF-8")?;
    let output = sb.run(&["--config", file, "config"], &[])?;
    assert_key_error(&output, "FILE-SENTINEL-VALUE");
    Ok(())
}

#[test]
fn config_is_read_only_and_needs_no_credentials() -> Result<()> {
    let sb = Sandbox::new()?;
    let db = sb.path().join("db").join("archive.sqlite");
    let db_arg = db.to_str().context("db path is not UTF-8")?;
    let before = snapshot(sb.path())?;

    let output = sb.run(&["--db", db_arg, "config"], &[])?;
    ensure!(output.status.success(), "{}", failure_text(&output));
    let value: Value = String::from_utf8(output.stdout)?.parse()?;
    assert_eq!(scalar(at(&value, &["model", "main"])?), "gpt-6-luna");

    let after = snapshot(sb.path())?;
    assert_eq!(before, after, "config changed the sandbox");
    assert!(!db.exists());
    assert!(!bubblepaw_core::Store::legacy_data_dir(&db).exists());
    assert!(!sb.path().join("home").exists());
    Ok(())
}

#[test]
fn storage_paths_retention_and_cli_precedence_are_layered_without_io() -> Result<()> {
    let sb = Sandbox::new()?;
    let path = sb.write("settings/storage.toml", "[storage]\ndata_dir = 'state'\nlegacy_db = 'archive.sqlite'\nfinished_event_retention_hours = 48\ntelemetry_retention_days = 14\n")?;
    let output = sb.run(&["--config", path.to_str().unwrap(), "config"], &[])?;
    ensure!(output.status.success(), "{}", failure_text(&output));
    let config: Value = String::from_utf8(output.stdout)?.parse()?;
    // Compare as native paths: Windows renders joined paths with backslashes.
    assert_eq!(
        PathBuf::from(config["storage"]["data_dir"].as_str().unwrap()),
        sb.path().join("settings").join("state")
    );
    assert_eq!(
        PathBuf::from(config["storage"]["legacy_db"].as_str().unwrap()),
        sb.path().join("settings").join("archive.sqlite")
    );
    assert_eq!(
        config["storage"]["finished_event_retention_hours"].as_integer(),
        Some(48)
    );
    assert_eq!(
        config["storage"]["telemetry_retention_days"].as_integer(),
        Some(14)
    );
    assert!(!sb.path().join("settings").join("state").exists());

    let output = sb.run(
        &[
            "--config",
            path.to_str().unwrap(),
            "--data-dir",
            "cli-state",
            "--set",
            "storage.data_dir=last-state",
            "config",
        ],
        &[("BUBBLEPAW_STORAGE__TELEMETRY_RETENTION_DAYS", "3")],
    )?;
    ensure!(output.status.success(), "{}", failure_text(&output));
    let config: Value = String::from_utf8(output.stdout)?.parse()?;
    assert_eq!(
        PathBuf::from(config["storage"]["data_dir"].as_str().unwrap()),
        sb.workspace().join("last-state")
    );
    assert_eq!(
        config["storage"]["telemetry_retention_days"].as_integer(),
        Some(3)
    );
    let defaults = sb.effective(&["config"])?;
    assert_eq!(
        defaults["storage"]["finished_event_retention_hours"].as_integer(),
        Some(24)
    );
    assert_eq!(
        defaults["storage"]["telemetry_retention_days"].as_integer(),
        Some(30)
    );
    let output = sb.run(
        &[
            "--config",
            path.to_str().unwrap(),
            "--set",
            "storage.telemetry_retention_days=-1",
            "config",
        ],
        &[],
    )?;
    assert!(!output.status.success());
    Ok(())
}
