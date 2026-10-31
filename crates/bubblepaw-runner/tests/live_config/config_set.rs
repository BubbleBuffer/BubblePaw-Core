//! `config.set`: validate, write the file in place, apply through the reload path.
use super::*;
use bubblepaw_runner::{
    config::LoadOptions,
    config_set::{Applied, ConfigSetError, edit_document},
    host_api::{HostCtx, Profile, dispatch},
    provider_setup::ProviderSetup,
};
use toml_edit::DocumentMut;

pub(super) fn setup(fixture: &Fixture) -> ProviderSetup {
    let inputs = LoadOptions {
        workspace: fixture._root.path().to_owned(),
        home: None,
        custom_file: Some(fixture.file.clone()),
        env: vec![],
        cli: vec![],
    };
    ProviderSetup::new(inputs, fixture.providers.startup_config())
        .unwrap()
        .with_live(fixture.providers.clone())
        .with_host_config(fixture.live.clone())
}

fn refusal(error: anyhow::Error) -> ConfigSetError {
    error
        .downcast::<ConfigSetError>()
        .expect("a typed config error")
}

#[tokio::test]
async fn a_live_key_reaches_its_subscriber_and_reports_live() {
    let fixture = Fixture::new(&config("")).await;
    let mut agents = fixture.providers.hub().agents();
    agents.borrow_and_update();
    let result = setup(&fixture)
        .config_set("agents.wake_debounce_ms", Some(&json!(77)))
        .await
        .unwrap();
    assert_eq!(result.applied, Applied::Live);
    assert!(result.reload.is_some_and(|report| report.applied));
    assert!(agents.has_changed().unwrap());
    assert_eq!(agents.borrow().wake_debounce_ms, 77);
    assert!(
        std::fs::read_to_string(&fixture.file)
            .unwrap()
            .contains("wake_debounce_ms = 77")
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn a_restart_only_key_is_saved_and_reported_as_restart_required() {
    let fixture = Fixture::new(&config("")).await;
    let result = setup(&fixture)
        .config_set("storage.archive_after_days", Some(&json!(42)))
        .await
        .unwrap();
    assert_eq!(result.applied, Applied::RestartRequired);
    let report = result.reload.unwrap();
    assert_eq!(report.restart_keys, ["storage.archive_after_days"]);
    assert!(
        std::fs::read_to_string(&fixture.file)
            .unwrap()
            .contains("archive_after_days = 42")
    );
    // The running host keeps its startup value.
    assert_eq!(
        fixture
            .providers
            .snapshot()
            .settings
            .storage
            .archive_after_days,
        30
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn a_refused_value_leaves_the_file_byte_identical() {
    let fixture = Fixture::new(&config("[agents]\nauto_wake=false\n")).await;
    let before = std::fs::read(&fixture.file).unwrap();
    let setup = setup(&fixture);
    let cases = [
        ("agents.max_depth", json!("deep"), "wrong_type"),
        ("agents.wake_debounce_ms", json!([1]), "wrong_type"),
        ("approvals.confirm", json!("maybe"), "invalid_value"),
        ("agents.stale_reminder_secs", json!(-1), "wrong_type"),
        ("no.such.key", json!(1), "unknown_key"),
        ("agents", json!({"auto_wake": true}), "unknown_key"),
        // A map entry that would be incomplete is refused whole.
        ("mcp.servers.half.enabled", json!(true), "invalid_value"),
    ];
    for (key, value, kind) in cases {
        let error = refusal(setup.config_set(key, Some(&value)).await.unwrap_err());
        assert_eq!(error.kind(), kind, "{key}: {error}");
        assert_eq!(std::fs::read(&fixture.file).unwrap(), before, "{key}");
    }
    fixture.shutdown().await;
}

#[tokio::test]
async fn comments_and_order_survive_a_set_and_null_restores_the_default() {
    let text = config(
        "# top comment\n[agents]\n# about debounce\nwake_debounce_ms = 13 # trailing\nauto_wake = false\n",
    );
    let fixture = Fixture::new(&text).await;
    let setup = setup(&fixture);
    setup
        .config_set("agents.wake_debounce_ms", Some(&json!(77)))
        .await
        .unwrap();
    let edited = std::fs::read_to_string(&fixture.file).unwrap();
    assert_eq!(edited, text.replace("= 13", "= 77"));
    // Null removes the key; the default applies live.
    let mut agents = fixture.providers.hub().agents();
    agents.borrow_and_update();
    let result = setup
        .config_set("agents.wake_debounce_ms", None)
        .await
        .unwrap();
    assert_eq!(result.applied, Applied::Live);
    let removed = std::fs::read_to_string(&fixture.file).unwrap();
    assert!(!removed.contains("wake_debounce_ms"));
    assert!(removed.contains("# top comment") && removed.contains("auto_wake = false"));
    assert!(agents.has_changed().unwrap());
    assert_ne!(agents.borrow().wake_debounce_ms, 77);
    fixture.shutdown().await;
}

#[tokio::test]
async fn setting_a_leaf_creates_a_map_entry_and_removing_every_leaf_drops_it() {
    let fixture = Fixture::new(&config("")).await;
    let setup = setup(&fixture);
    setup
        .config_set("providers.extra.kind", Some(&json!("scripted")))
        .await
        .unwrap();
    let text = std::fs::read_to_string(&fixture.file).unwrap();
    assert!(
        text.contains("[providers.extra]\nkind = \"scripted\""),
        "{text}"
    );
    setup
        .config_set("providers.extra.kind", None)
        .await
        .unwrap();
    let text = std::fs::read_to_string(&fixture.file).unwrap();
    assert!(!text.contains("extra"), "{text}");
    fixture.shutdown().await;
}

#[tokio::test]
async fn null_removes_a_whole_map_entry_but_a_plain_table_refuses_it() {
    let command = toml::Value::String(env!("CARGO_BIN_EXE_bubblepaw-runner-fake-mcp").into());
    let text = config(&format!(
        "[toolsets.looker]\ntools=['category:files']\n\
         # keep me\n[toolsets.other]\ntools=['read']\n\
         [mcp.servers.late]\ncommand={command}\napproval='none'\n"
    ));
    let fixture = Fixture::new(&text).await;
    assert_eq!(fixture.mcp.servers().len(), 1);
    let setup = setup(&fixture);
    // A table that is not a map entry keeps refusing null; the file is untouched.
    let before = std::fs::read(&fixture.file).unwrap();
    for key in ["agents", "mcp", "mcp.servers", "toolsets"] {
        let error = refusal(setup.config_set(key, None).await.unwrap_err());
        assert_eq!(error.kind(), "unknown_key", "{key}");
        assert_eq!(std::fs::read(&fixture.file).unwrap(), before, "{key}");
    }
    // A value for an entry key stays refused: only null removes it.
    let error = refusal(
        setup
            .config_set("toolsets.looker", Some(&json!({"tools": ["read"]})))
            .await
            .unwrap_err(),
    );
    assert_eq!(error.kind(), "unknown_key");

    setup.config_set("toolsets.looker", None).await.unwrap();
    let text = std::fs::read_to_string(&fixture.file).unwrap();
    assert!(!text.contains("looker") && text.contains("[toolsets.other]"));
    assert!(text.contains("# keep me"), "{text}");
    // Removing an absent entry is a no-op, not an error.
    let result = setup.config_set("toolsets.looker", None).await.unwrap();
    assert!(result.reload.is_none());

    // The MCP server stops and its tools go with it, in one call.
    let result = setup.config_set("mcp.servers.late", None).await.unwrap();
    assert!(result.reload.is_some_and(|report| report.applied));
    let text = std::fs::read_to_string(&fixture.file).unwrap();
    assert!(!text.contains("late") && !text.contains("mcp"), "{text}");
    assert!(fixture.mcp.servers().is_empty());
    assert!(
        fixture
            .live
            .main_registry()
            .get("mcp__late__echo")
            .is_none()
    );
    fixture.shutdown().await;
}

#[test]
fn edit_document_removes_entries_with_null_only() {
    let mut doc: DocumentMut = "[providers.a]
kind='scripted'
[providers.b]
kind='scripted'
[skills]
enabled=true
[skills.one]
enabled=false
"
    .parse()
    .unwrap();
    edit_document(&mut doc, "providers.a", None).unwrap();
    edit_document(&mut doc, "skills.one", None).unwrap();
    let text = doc.to_string();
    assert!(text.contains("[providers.b]") && !text.contains("providers.a"));
    assert!(!text.contains("one") && text.contains("[skills]"));
    for key in ["providers", "skills", "agents"] {
        assert_eq!(
            edit_document(&mut doc, key, None).unwrap_err().kind(),
            "unknown_key",
            "{key}"
        );
    }
    assert_eq!(
        edit_document(&mut doc, "providers.b", Some(&json!({"kind": "x"})))
            .unwrap_err()
            .kind(),
        "unknown_key"
    );
}

#[test]
fn edit_document_is_pure_and_in_place() {
    let mut doc: DocumentMut = "a = 1\n[agents]\n# keep\nauto_wake = true\n"
        .parse()
        .unwrap();
    edit_document(&mut doc, "agents.auto_wake", Some(&json!(false))).unwrap();
    assert_eq!(
        doc.to_string(),
        "a = 1\n[agents]\n# keep\nauto_wake = false\n"
    );
    // Removing an absent key is a no-op.
    let before = doc.to_string();
    edit_document(&mut doc, "agents.max_depth", None).unwrap();
    assert_eq!(doc.to_string(), before);
    assert_eq!(
        edit_document(&mut doc, "agents.nope", Some(&json!(1)))
            .unwrap_err()
            .kind(),
        "unknown_key"
    );
}

#[tokio::test]
async fn the_host_methods_list_set_and_gate_settings() {
    let fixture = Fixture::new(&config("")).await;
    let mut ctx = HostCtx::for_store(fixture.store.clone(), fixture._root.path().to_owned());
    ctx.providers = Some(setup(&fixture));
    ctx.live_config = Some(fixture.live.clone());
    ctx.provider_config = Some(fixture.providers.clone());
    let entry = |reply: &serde_json::Value, key: &str| {
        reply["settings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["key"] == key)
            .cloned()
            .unwrap_or_else(|| panic!("no {key} in {reply}"))
    };
    let get = |key: Option<&str>| {
        let ctx = ctx.clone();
        let params = key.map_or(json!({}), |key| json!({"key": key}));
        async move { dispatch(&ctx, Profile::Read, "config.get", params).await }
    };
    // Read lists every leaf with its liveness; one key answers just that key.
    let all = get(None).await.unwrap();
    assert_eq!(entry(&all, "agents.auto_wake")["live"], true);
    assert_eq!(entry(&all, "storage.archive_after_days")["live"], false);
    let one = get(Some("agents.wake_debounce_ms")).await.unwrap();
    assert_eq!(one["settings"].as_array().unwrap().len(), 1);
    assert!(
        get(Some("agents")).await.unwrap()["settings"]
            .as_array()
            .unwrap()
            .len()
            > 1
    );
    assert!(get(Some("no.such")).await.is_err());
    // Set needs the Full profile; a refused caller changes nothing.
    let before = std::fs::read(&fixture.file).unwrap();
    for caller in [Profile::Read, Profile::Edit] {
        let params = json!({"key": "agents.auto_wake", "value": false});
        let error = dispatch(&ctx, caller, "config.set", params)
            .await
            .unwrap_err();
        assert_eq!(error.rpc_code(), -32003);
    }
    assert_eq!(std::fs::read(&fixture.file).unwrap(), before);
    let live = dispatch(
        &ctx,
        Profile::Full,
        "config.set",
        json!({"key": "agents.auto_wake", "value": false}),
    )
    .await
    .unwrap();
    assert_eq!(live["applied"], "live");
    assert_eq!(
        entry(&get(None).await.unwrap(), "agents.auto_wake")["value"],
        false
    );
    let restart = dispatch(
        &ctx,
        Profile::Full,
        "config.set",
        json!({"key": "storage.archive_after_days", "value": 7}),
    )
    .await
    .unwrap();
    assert_eq!(restart["applied"], "restart_required");
    // The file holds 7; the running host keeps 30 and says a restart is pending.
    let waiting = entry(&get(None).await.unwrap(), "storage.archive_after_days");
    assert_eq!(
        (
            waiting["value"].as_u64(),
            waiting["pending_restart"].as_bool()
        ),
        (Some(30), Some(true))
    );
    let error = dispatch(
        &ctx,
        Profile::Full,
        "config.set",
        json!({"key": "agents.max_depth", "value": "x"}),
    )
    .await
    .unwrap_err();
    assert_eq!((error.rpc_code(), error.http_status()), (-32011, 400));
    fixture.shutdown().await;
}
