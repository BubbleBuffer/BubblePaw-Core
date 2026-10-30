//! A custom first-run base survives an earlier default preview and apply.
use super::*;

#[tokio::test]
async fn first_run_custom_base_survives_apply_after_default_template_preview() {
    let mut fixture = Fixture::new_with_layout(&config(""), true).await;
    let worker = install_compaction(&mut fixture);
    let memory = fixture._root.path().join("AGENTS.md");
    std::fs::write(&memory, "CUSTOM-BASE-MEMORY-V1").unwrap();
    skill(&fixture, "custom", "CUSTOM-BASE-CATALOG-V1");
    let preview = fixture.harness.request_template(&fixture.session).unwrap();
    assert!(
        preview
            .system_prompt
            .contains("BASE-COMPACTION-INSTRUCTIONS")
    );
    let run = fixture
        .harness
        .prompt(
            &fixture.session,
            "first run with chosen base",
            RunOptions {
                system_prompt: Some("USER-SELECTED-CUSTOM-BASE".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(run.status, "completed", "{:?}", run.error);
    let bound = fixture.harness.request_template(&fixture.session).unwrap();
    assert!(bound.system_prompt.contains("USER-SELECTED-CUSTOM-BASE"));
    assert!(!bound.system_prompt.contains("BASE-COMPACTION-INSTRUCTIONS"));
    std::fs::write(&memory, "CUSTOM-BASE-MEMORY-V2").unwrap();
    skill(&fixture, "custom", "CUSTOM-BASE-CATALOG-V2");
    fixture.turn("custom base changes announced").await;
    fixture.turn("retain custom base current turn").await;
    fixture
        .harness
        .compact_session(&fixture.session, None)
        .await
        .unwrap();
    let applied = fixture.harness.request_template(&fixture.session).unwrap();
    assert!(applied.system_prompt.contains("USER-SELECTED-CUSTOM-BASE"));
    assert!(
        !applied
            .system_prompt
            .contains("BASE-COMPACTION-INSTRUCTIONS")
    );
    assert!(applied.system_prompt.contains("CUSTOM-BASE-MEMORY-V2"));
    assert!(applied.system_prompt.contains("CUSTOM-BASE-CATALOG-V2"));
    assert!(!applied.system_prompt.contains("CUSTOM-BASE-MEMORY-V1"));
    assert_eq!(
        serde_json::to_vec(&bound.tools).unwrap(),
        serde_json::to_vec(&applied.tools).unwrap()
    );
    fixture.turn("serve applied custom base").await;
    assert_served_prefix(&fixture, &applied);
    fixture.shutdown().await;
    drop(worker);
}
