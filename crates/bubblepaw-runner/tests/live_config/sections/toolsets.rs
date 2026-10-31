//! Named toolsets and per-skill switches: both follow `config.set` without a restart.
use super::config_set::setup;
use super::*;
use bubblepaw_runner::config_set::ConfigSetError;

fn has(registry: &ToolRegistry, name: &str) -> bool {
    registry.get(name).is_some()
}

#[tokio::test]
async fn agent_toolset_narrows_the_main_registry_live_and_keeps_children_whole() {
    let fixture = Fixture::new(&config(
        "[toolsets.looker]\ntools=['category:files','web_fetch']\n",
    ))
    .await;
    assert!(has(&fixture.live.main_registry(), "exec"));
    let setup = setup(&fixture);
    let result = setup
        .config_set("agent.toolset", Some(&json!("looker")))
        .await
        .unwrap();
    assert!(!result.reload.unwrap().restart_needed);
    let main = fixture.live.main_registry();
    assert!(has(&main, "read") && has(&main, "edit"));
    assert!(!has(&main, "exec"));
    assert!(main.is_unavailable("exec"));
    assert!(has(&fixture.live.registry(), "exec"));
    setup
        .config_set("agent.toolset", Some(&json!("read-only")))
        .await
        .unwrap();
    let main = fixture.live.main_registry();
    assert!(has(&main, "read") && !has(&main, "edit") && !has(&main, "exec"));
    setup
        .config_set("agent.toolset", Some(&json!("all")))
        .await
        .unwrap();
    assert!(has(&fixture.live.main_registry(), "exec"));
    fixture.shutdown().await;
}

#[tokio::test]
async fn a_category_entry_picks_up_a_tool_added_later() {
    let fixture = Fixture::new(&config(
        "[agent]\ntoolset='mcp-only'\n[toolsets.mcp-only]\ntools=['category:mcp:late']\n",
    ))
    .await;
    assert!(!has(&fixture.live.main_registry(), "read"));
    let command = toml::Value::String(env!("CARGO_BIN_EXE_bubblepaw-runner-fake-mcp").into());
    fixture.write(&config(&format!(
        "[agent]\ntoolset='mcp-only'\n[toolsets.mcp-only]\ntools=['category:mcp:late']\n\
         [mcp.servers.late]\ncommand={command}\napproval='none'\n"
    )));
    let report = fixture.live.reload(None).await;
    assert!(report.applied, "{report:?}");
    let main = fixture.live.main_registry();
    assert!(has(&main, "mcp__late__echo"), "{:?}", main.entries());
    assert!(!has(&main, "read"));
    let listed = fixture.live.tool_list();
    let echo = listed
        .iter()
        .find(|tool| tool.name == "mcp__late__echo")
        .unwrap();
    assert_eq!(
        (echo.category.as_str(), echo.source.as_str()),
        ("mcp:late", "mcp")
    );
    assert!(
        listed
            .iter()
            .any(|tool| tool.name == "read" && tool.category == "files")
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn built_in_toolset_names_and_unknown_choices_are_refused() {
    let fixture = Fixture::new(&config("")).await;
    let before = std::fs::read(&fixture.file).unwrap();
    let setup = setup(&fixture);
    for (key, value) in [
        ("toolsets.all.tools", json!(["read"])),
        ("toolsets.read-only.tools", json!(["read"])),
        ("toolsets.bad.tools", json!(["category:nope"])),
        ("agent.toolset", json!("missing")),
    ] {
        let error = setup.config_set(key, Some(&value)).await.unwrap_err();
        assert!(error.downcast_ref::<ConfigSetError>().is_some(), "{key}");
        assert_eq!(std::fs::read(&fixture.file).unwrap(), before, "{key}");
    }
    fixture.shutdown().await;
}

#[tokio::test]
async fn a_skill_switch_removes_and_restores_it_in_the_next_catalog() {
    let external = tempfile::tempdir().unwrap();
    let dir = external.path().join("switchable");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: switchable\ndescription: Can be turned off\n---\nBODY\n",
    )
    .unwrap();
    let extra = format!(
        "[skills]\ndirs=[{}]\n",
        toml::Value::String(external.path().to_string_lossy().into_owned())
    );
    let fixture = Fixture::new(&config(&extra)).await;
    fixture.turn("first").await;
    let listed = fixture.live.skill_list().unwrap();
    assert!(
        listed
            .skills
            .iter()
            .any(|skill| skill.name == "switchable" && skill.enabled)
    );
    let setup = setup(&fixture);
    setup
        .config_set("skills.switchable.enabled", Some(&json!(false)))
        .await
        .unwrap();
    fixture.turn("second").await;
    assert_eq!(fixture.notices().len(), 1);
    assert!(
        fixture.notices()[0].contains("removed") && fixture.notices()[0].contains("switchable")
    );
    let listed = fixture.live.skill_list().unwrap();
    assert!(
        listed
            .skills
            .iter()
            .any(|skill| skill.name == "switchable" && !skill.enabled)
    );
    setup
        .config_set("skills.switchable.enabled", Some(&json!(true)))
        .await
        .unwrap();
    fixture.turn("third").await;
    assert_eq!(fixture.notices().len(), 2);
    assert!(fixture.notices()[1].contains("switchable"));
    fixture.shutdown().await;
}
