//! Typed config reaches the host/session boundaries without any live provider.
use anyhow::{Result, ensure};
use bubblepaw_core::{ContextStrategy, Role, Store};
#[path = "support/message_fixture.rs"]
mod message_fixture;
use bubblepaw_runner::context_strategies::{typed_user_block, typed_user_block_with_budget};
#[cfg(feature = "bench")]
use bubblepaw_runner::{
    episode_models::EpisodeModelConfig,
    pricing::PriceTable,
    provider::ProviderEndpoints,
    replay::{ReplayCorpus, ReplayOptions, run_corpus_with_context_config},
    summarization::SummaryConfig,
};
use message_fixture::message;
use serde_json::{Value, json};
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
};

#[path = "config_runtime/responses.rs"]
mod responses;

fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bubblepaw"));
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("BUBBLEPAW_") {
            command.env_remove(name);
        }
    }
    for name in ["OPENAI_API_KEY", "OPENCODE_API_KEY", "OPENROUTER_API_KEY"] {
        command.env_remove(name);
    }
    command
        .args(["--workspace"])
        .arg(root)
        .arg("--db")
        .arg(root.join("archive.sqlite"));
    command
}

#[tokio::test]
async fn messages_role_and_registry_route_through_cli() -> Result<()> {
    use axum::{Json, Router, routing::post};
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let seen = requests.clone();
    let app = Router::new().route("/v1/messages", post(move |Json(body): Json<Value>| {
        let seen = seen.clone();
        async move {
            seen.lock().unwrap().push(body);
            Json(json!({"type":"message","role":"assistant","content":[{"type":"text","text":"messages fixture complete"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":2}}))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}/v1", listener.local_addr()?);
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let root = tempfile::tempdir()?;
    let config = root.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[providers.opencode-messages]\nbase_url='{base}'\nkey_env='BUBBLEPAW_TEST_MESSAGES_KEY'\n[context]\nauto_summarize=false\n"
        ),
    )?;
    let role = "roles.summary=[\"opencode-messages/claude-haiku-5-5\"]";
    let output = command(root.path())
        .arg("--config")
        .arg(&config)
        .args(["--provider", "demo", "--set", role, "config"])
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let printed: toml::Value = toml::from_str(&String::from_utf8(output.stdout)?)?;
    assert_eq!(
        printed["providers"]["opencode-messages"]["kind"].as_str(),
        Some("anthropic-messages")
    );
    assert_eq!(
        printed["roles"]["summary"]["models"][0].as_str(),
        Some("opencode-messages/claude-haiku-5-5")
    );
    // Config output preserves role references; typed settings resolve them.
    assert_eq!(
        printed["roles"]["chapter"]["models"][0].as_str(),
        Some("summary")
    );
    assert!(!root.path().join("archive.sqlite").exists());

    let mut cli = command(root.path());
    cli.arg("--config")
        .arg(&config)
        .args([
            "--model",
            "opencode-messages/claude-haiku-5-5",
            "--set",
            role,
            "run",
            "hello",
        ])
        .env("BUBBLEPAW_TEST_MESSAGES_KEY", "local-fixture-key")
        .env("CODEX_HOME", root.path().join("absent-codex"));
    let output = tokio::process::Command::from(cli).output().await?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut configured = std::fs::read_to_string(&config)?;
    configured.push_str("\n[model]\nmain='opencode-messages/claude-haiku-5-5'\n");
    std::fs::write(&config, configured)?;
    let mut cli = command(root.path());
    cli.arg("--config")
        .arg(&config)
        .args(["run", "config model hello"])
        .env("BUBBLEPAW_TEST_MESSAGES_KEY", "local-fixture-key")
        .env("CODEX_HOME", root.path().join("absent-codex"));
    let second = tokio::process::Command::from(cli).output().await?;
    server.abort();
    ensure!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1]["model"], "claude-haiku-5-5");
    assert_eq!(requests[1]["output_config"]["effort"], "low");
    assert_eq!(requests[0]["model"], "claude-haiku-5-5");
    assert_eq!(requests[0]["thinking"]["type"], "adaptive");
    assert_eq!(requests[0]["output_config"]["effort"], "low");
    assert!(String::from_utf8_lossy(&output.stdout).contains("messages fixture complete"));
    Ok(())
}

fn fixture_config(root: &Path, strategy: Option<&str>) -> Result<std::path::PathBuf> {
    let path = root.join("config.toml");
    // Qualified configured main models do not require unrelated Codex credentials.
    let mut config = "[model]\nmain='local/fixture'\n[providers.local]\nkind='scripted'\n[context]\nauto_summarize=false\n".to_owned();
    if let Some(strategy) = strategy {
        config.push_str(&format!("strategy='{strategy}'\n"));
    }
    config.push_str("[approvals]\nread_only=true\n");
    std::fs::write(&path, config)?;
    Ok(path)
}

#[cfg(feature = "bench")]
#[tokio::test]
async fn messages_replay_main_resolves_without_calling_the_recorded_turn_provider() -> Result<()> {
    use axum::{Json, Router, routing::post};
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let seen = requests.clone();
    let app = Router::new().route(
        "/v1/messages",
        post(move |Json(body): Json<Value>| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push(body);
                Json(json!({"error":"recorded turns must not call the main provider"}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}/v1", listener.local_addr()?);
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let root = tempfile::tempdir()?;
    let config = root.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[model]\nmain='opencode-messages/claude-haiku-5-5'\n[providers.opencode-messages]\nbase_url='{base}'\nkey_env='BUBBLEPAW_TEST_MESSAGES_KEY'\n[roles]\ndecider=['llm/main']\n"
        ),
    )?;
    let corpus = root.path().join("corpus.json");
    std::fs::write(
        &corpus,
        serde_json::to_vec(&json!({
            "chat_id":"messages-primary", "instructions":"fixture",
            "messages":[message("u1", Role::User, "hello".into()), message("a1", Role::Assistant, "recorded answer".into())]
        }))?,
    )?;
    for (name, cli_model) in [("configured", false), ("cli", true)] {
        let out = root.path().join(name);
        let mut cli = command(root.path());
        cli.arg("--config").arg(&config);
        if cli_model {
            cli.args(["--model", "opencode-messages/claude-haiku-5-5"]);
        }
        cli.args(["replay", "--chat"])
            .arg(&corpus)
            .arg("--out")
            .arg(&out)
            .arg("--dry-run")
            .env("BUBBLEPAW_TEST_MESSAGES_KEY", "local-fixture-key")
            .env("CODEX_HOME", root.path().join("absent-codex"));
        let output = tokio::process::Command::from(cli).output().await?;
        ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value = serde_json::from_slice(&std::fs::read(out.join("report.json"))?)?;
        assert_eq!(report["status"], "complete");
    }
    // The priced Haiku address is also accepted through llm/main. Recorded
    // history and dry mode still must not invoke that provider.
    let mut cli = command(root.path());
    cli.arg("--config")
        .arg(&config)
        .args(["--set", "roles.decider=[\"llm/main\"]", "replay", "--chat"])
        .arg(&corpus)
        .arg("--out")
        .arg(root.path().join("priced-decider"))
        .arg("--dry-run");
    let output = tokio::process::Command::from(cli).output().await?;
    server.abort();
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&std::fs::read(
        root.path().join("priced-decider/report.json"),
    )?)?;
    assert_eq!(report["status"], "complete");
    assert_eq!(report["mode"], "scripted");
    assert_eq!(report["messages_replayed"], 2);
    assert!(report["calls"].as_array().unwrap().is_empty());
    assert!(requests.lock().unwrap().is_empty());
    // Only canonical corpus JSON is accepted; other recorded-chat inputs fail
    // before any output directory is created.
    let raw = root.path().join("raw-chat.jsonl");
    std::fs::write(&raw, "{\"type\":\"message\"}\n")?;
    let rejected = root.path().join("rejected");
    let mut cli = command(root.path());
    cli.arg("--config")
        .arg(&config)
        .args(["replay", "--chat"])
        .arg(&raw)
        .arg("--out")
        .arg(&rejected)
        .arg("--dry-run")
        .env("BUBBLEPAW_TEST_MESSAGES_KEY", "local-fixture-key")
        .env("CODEX_HOME", root.path().join("absent-codex"));
    let output = tokio::process::Command::from(cli).output().await?;
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("only canonical corpus JSON"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!rejected.exists());
    Ok(())
}

#[test]
fn removed_context_strategy_config_rejects_before_session_creation() -> Result<()> {
    let root = tempfile::tempdir()?;
    let config = fixture_config(root.path(), Some("minimal"))?;
    let output = command(root.path())
        .arg("--config")
        .arg(config)
        .args(["run", "hello"])
        .output()?;
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("context.strategy=\"minimal\""), "{error}");
    assert!(error.contains("expected lcm or classical"), "{error}");
    assert!(!root.path().join("archive.sqlite").exists());
    Ok(())
}

#[test]
fn configured_strategy_creates_session_and_explicit_mismatch_rejects_resume() -> Result<()> {
    let root = tempfile::tempdir()?;
    let config = fixture_config(root.path(), Some("classical"))?;
    let output = command(root.path())
        .arg("--config")
        .arg(&config)
        .args(["run", "hello"])
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let store = Store::open(root.path().join("archive.sqlite"))?;
    let session = store.list_sessions()?.remove(0);
    assert_eq!(session.context_strategy, ContextStrategy::Classical);
    drop(store);
    let output = command(root.path())
        .arg("--config")
        .arg(&config)
        .args([
            "--set",
            "context.strategy=lcm",
            "run",
            "continue",
            "--session",
            &session.id,
        ])
        .output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Context strategy is fixed"));
    let store = Store::open(root.path().join("archive.sqlite"))?;
    assert_eq!(
        store.session(&session.id)?.context_strategy,
        ContextStrategy::Classical
    );
    Ok(())
}

#[test]
fn unconfigured_strategy_resume_keeps_the_saved_selection() -> Result<()> {
    let root = tempfile::tempdir()?;
    let config = fixture_config(root.path(), None)?;
    let store = Store::open(root.path().join("archive.sqlite"))?;
    store.bind_workspace(root.path())?;
    let session = store.create_session_with_strategy("existing", ContextStrategy::Classical)?;
    drop(store);
    let output = command(root.path())
        .arg("--config")
        .arg(&config)
        .args(["run", "hello", "--session", &session.id])
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        Store::open(root.path().join("archive.sqlite"))?
            .session(&session.id)?
            .context_strategy,
        ContextStrategy::Classical
    );
    Ok(())
}

#[test]
fn stdio_session_open_uses_configured_strategy() -> Result<()> {
    let root = tempfile::tempdir()?;
    let config = fixture_config(root.path(), Some("classical"))?;
    let mut child = command(root.path())
        .arg("--config")
        .arg(&config)
        .arg("stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"session.open\",\"params\":{}}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"shutdown\"}\n")?;
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = String::from_utf8(output.stdout)?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .find(|record| record["id"] == 1)
        .unwrap();
    assert_eq!(result["result"]["context_strategy"], "classical");
    Ok(())
}

#[test]
fn configured_verbatim_budget_changes_text_retention_and_keeps_ids() -> Result<()> {
    let history = vec![
        message("user-old", Role::User, "old-detail ".repeat(100)),
        message("user-new", Role::User, "new-detail ".repeat(100)),
    ];
    let tokenizer = tiktoken_rs::bpe_for_tokenizer(tiktoken_rs::tokenizer::Tokenizer::O200kBase)?;
    let defaults = typed_user_block(&history, tokenizer);
    let small = typed_user_block_with_budget(&history, tokenizer, 64);
    assert!(defaults.contains("new-detail") && defaults.contains("old-detail"));
    assert!(!small.contains("new-detail") && !small.contains("old-detail"));
    assert!(small.contains("user-new") && small.contains("user-old"));
    Ok(())
}

#[cfg(feature = "bench")]
#[tokio::test]
async fn replay_uses_configured_cap_with_the_recorded_window_reserve() -> Result<()> {
    let root = tempfile::tempdir()?;
    let report = run_corpus_with_context_config(
        ReplayCorpus {
            chat_id: "config-threshold".into(),
            recorded_model: None,
            context_window: Some(60_000),
            instructions: "fixture".into(),
            messages: vec![
                message("u1", Role::User, "hello".into()),
                message("a1", Role::Assistant, "done".into()),
            ],
            message_metadata: json!([]),
            boundary_gold: None,
            relates_gold: vec![],
            provenance: json!({"fixture":true}),
        },
        ReplayOptions {
            prices: PriceTable::shared(),
            agents: Default::default(),
            chat: "fixture".into(),
            out: root.path().into(),
            threshold: None,
            dry_run: true,
            context_strategy: ContextStrategy::Classical,
            probes: None,
            checkpoints_at_turns: vec![],
            probe_plan: None,
            plan_probes: false,
            capture_turn_frontiers: false,
            probe_limit: None,
            max_cost_usd: None,
        },
        EpisodeModelConfig::default(),
        ProviderEndpoints::default(),
        SummaryConfig {
            threshold_cap: 50_000,
            verbatim_user_tokens: 64,
            ..Default::default()
        },
    )
    .await?;
    // docs/lcm-contract.md "Apply": the configured cap is physical; the effective
    // cap min(60k window, 50k) loses the 16k reserve exactly once.
    assert_eq!(report["threshold"], 34_000);
    assert_eq!(report["status"], "complete");
    assert_eq!(report["totals"]["input_tokens"], 0);
    Ok(())
}

#[tokio::test]
async fn configured_free_main_falls_through_403_without_replaying_task_and_persists_route()
-> Result<()> {
    use axum::{
        Json, Router,
        http::{HeaderMap, StatusCode},
        routing::post,
    };
    let requests = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
    let seen = requests.clone();
    let app=Router::new().route("/v1/chat/completions", post(move |headers:HeaderMap,Json(body):Json<Value>| {
        let seen=seen.clone();
        async move {
            let model=body["model"].as_str().unwrap();
            seen.lock().unwrap().push((model.into(),headers["x-opencode-session"].to_str().unwrap().into()));
            if model.ends_with("-free") {
                (StatusCode::FORBIDDEN,Json(json!({"error":{"type":"FreeTierError"}})))
            } else {
                (StatusCode::OK,Json(json!({"choices":[{"message":{"content":"fallback fixture complete"},"finish_reason":"stop"}],"usage":{"prompt_tokens":11,"completion_tokens":2}})))
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}/v1", listener.local_addr()?);
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let root = tempfile::tempdir()?;
    let config = root.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[model]\nmain='opencode-free/deepseek-v4.1-flash-free'\nfallbacks=['opencode/deepseek-v4.1-flash']\n[providers.opencode-free]\nkind='chat-completions'\nbase_url='{base}'\nkey_env='BUBBLEPAW_TEST_ROUTE_KEY'\n[providers.opencode]\nkind='chat-completions'\nbase_url='{base}'\nkey_env='BUBBLEPAW_TEST_ROUTE_KEY'\n[context]\nauto_summarize=false\n"
        ),
    )?;
    let mut cli = command(root.path());
    cli.arg("--config")
        .arg(&config)
        .args(["run", "hello"])
        .env("BUBBLEPAW_TEST_ROUTE_KEY", "synthetic");
    let output = tokio::process::Command::from(cli).output().await?;
    server.abort();
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("fallback fixture complete"));
    let store = Store::open(root.path().join("archive.sqlite"))?;
    let session = store.list_sessions()?.remove(0);
    let history = store.messages(&session.id)?;
    assert_eq!(history.iter().filter(|m| m.role == Role::User).count(), 1);
    let answer = history.iter().find(|m| m.role == Role::Assistant).unwrap();
    assert_eq!(
        answer.provider_state.as_ref().unwrap()["_bubblepaw_selected_model"],
        "opencode/deepseek-v4.1-flash"
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0],
        ("deepseek-v4.1-flash-free".into(), session.id.clone())
    );
    assert_eq!(requests[1], ("deepseek-v4.1-flash".into(), session.id));
    Ok(())
}
