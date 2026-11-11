//! Automatic Classical apply through the ordinary host turn.
use super::*;

#[tokio::test]
async fn classical_automatic_apply_rebinds_current_prefix_and_discards_stale_pending_diff() {
    let mut fixture = Fixture::new_with_layout(&config(""), true).await;
    fixture.session = fixture
        .store
        .create_session_with_strategy("classical host", bubblepaw_core::ContextStrategy::Classical)
        .unwrap()
        .id;
    let worker = install_compaction(&mut fixture);
    let memory = fixture._root.path().join("AGENTS.md");
    std::fs::write(&memory, "CLASSICAL-ORIGINAL-A").unwrap();
    skill(&fixture, "classical", "CLASSICAL-CATALOG-A");
    fixture.turn("bind classical original prefix").await;
    let original = fixture.harness.request_template(&fixture.session).unwrap();
    let original_memory = fixture
        .store
        .attachment(&fixture.session, "bubblepaw-memory-files-v1")
        .unwrap();
    let original_skills = fixture
        .store
        .attachment(&fixture.session, "bubblepaw-skills-v1")
        .unwrap();
    assert!(original_memory.is_some() && original_skills.is_some());
    std::fs::write(&memory, "CLASSICAL-DELIVERED-BEFORE-PRESSURE").unwrap();
    skill(&fixture, "classical", "CLASSICAL-DELIVERED-CATALOG");
    skill(
        &fixture,
        "classical-added",
        "CLASSICAL-NEW-SKILL-DESCRIPTION",
    );
    let added_path = fixture
        ._root
        .path()
        .join(".agents/skills/classical-added/SKILL.md")
        .canonicalize()
        .unwrap();
    assert!(!original.system_prompt.contains("classical-added"));
    fixture
        .turn("deliver classical changes before large completed spans")
        .await;
    let delivered_notices = host_notices(&fixture);
    assert_eq!(delivered_notices.len(), 1);
    assert!(delivered_notices[0].content.contains("classical-added"));
    assert!(
        delivered_notices[0]
            .content
            .contains("CLASSICAL-DELIVERED-BEFORE-PRESSURE")
    );
    assert!(
        delivered_notices[0]
            .content
            .contains("CLASSICAL-DELIVERED-CATALOG")
    );
    drop(worker);

    let message = |role, content: String| bubblepaw_core::Message {
        id: uuid::Uuid::new_v4().to_string(),
        session_id: fixture.session.clone(),
        role,
        content,
        tool_calls: vec![],
        tool_call_id: None,
        origin: None,
        provider_state: None,
        is_error: false,
        created_at: "2026-10-30T12:00:00Z".into(),
    };
    // The Classical worker independently retains a 32k source tail even when
    // serving requests use a smaller raw-tail target. Each older assistant span
    // exceeds that floor; the 80k cap accommodates one retained span plus host
    // declarations, while all three spans together force an automatic apply.
    for index in 0..3 {
        fixture
            .store
            .append_batch(&[
                message(Role::User, format!("classical completed task {index}")),
                message(Role::Assistant, " datum".repeat(36_000)),
            ])
            .unwrap();
    }
    fixture
        .store
        .append_batch(&[
            message(Role::User, "small recent classical turn".into()),
            message(Role::Assistant, "recent detail ".repeat(200)),
        ])
        .unwrap();
    // The ordinary host refresh sees B and queues a diff. An edit to C lands
    // before preparation: successful apply must absorb that pending B diff.
    std::fs::write(&memory, "CLASSICAL-PENDING-B").unwrap();
    skill(&fixture, "classical", "CLASSICAL-CATALOG-B");
    let catalog_path = fixture
        ._root
        .path()
        .join(".agents/skills/classical/SKILL.md");
    let worker = install_context(
        &mut fixture,
        SummaryConfig {
            threshold_cap: 80_000,
            lcm: bubblepaw_runner::summarization::LcmConfig {
                raw_tail_tokens: 256,
                output_reserve_tokens: 0,
                ..Default::default()
            },
            ..Default::default()
        },
        true,
        Some(vec![
            (memory, "CLASSICAL-APPLIED-C".into()),
            (
                catalog_path,
                "---\nname: classical\ndescription: CLASSICAL-CATALOG-C\n---\nbody\n".into(),
            ),
        ]),
    );
    let history_before = fixture.store.messages(&fixture.session).unwrap();
    fixture.turn("trigger automatic classical apply").await;
    assert!(
        fixture
            .store
            .context_view(&fixture.session)
            .unwrap()
            .is_some()
    );
    let applied = fixture.harness.request_template(&fixture.session).unwrap();
    assert!(applied.system_prompt.contains("CLASSICAL-APPLIED-C"));
    assert!(applied.system_prompt.contains("CLASSICAL-CATALOG-C"));
    assert!(applied.system_prompt.contains("classical-added"));
    assert!(
        applied
            .system_prompt
            .contains("CLASSICAL-NEW-SKILL-DESCRIPTION")
    );
    assert!(
        applied
            .system_prompt
            .contains(&json!(added_path).to_string())
    );
    assert!(
        applied
            .system_prompt
            .contains("BASE-COMPACTION-INSTRUCTIONS")
    );
    assert!(!applied.system_prompt.contains("CLASSICAL-ORIGINAL-A"));
    assert!(!applied.system_prompt.contains("CLASSICAL-CATALOG-A"));
    assert_eq!(
        serde_json::to_vec(&applied.tools).unwrap(),
        serde_json::to_vec(&original.tools).unwrap()
    );
    assert_served_prefix(&fixture, &applied);
    assert_eq!(
        host_notices(&fixture),
        delivered_notices,
        "successful apply absorbs pending host diffs"
    );
    let frontier = fixture
        .store
        .context_view(&fixture.session)
        .unwrap()
        .unwrap()
        .frontier
        .unwrap();
    for message in &delivered_notices {
        assert!(
            fixture
                .store
                .message_seq(&fixture.session, &message.id)
                .unwrap()
                <= frontier
        );
    }
    {
        let requests = fixture.script.requests.lock().unwrap();
        for message in &requests.last().unwrap().messages {
            assert!(
                !delivered_notices
                    .iter()
                    .any(|delivered| delivered.id == message.id)
            );
            assert!(!message.content.contains("CLASSICAL-PENDING-B"));
            assert!(!message.content.contains("CLASSICAL-CATALOG-B"));
            assert!(!message.content.contains("Memory files changed"));
            assert!(!message.content.contains("Skills changed"));
        }
    }
    let history_after = fixture.store.messages(&fixture.session).unwrap();
    assert_eq!(
        &history_after[..history_before.len()],
        history_before.as_slice()
    );
    assert_eq!(
        fixture
            .store
            .attachment(&fixture.session, "bubblepaw-memory-files-v1")
            .unwrap(),
        original_memory
    );
    assert_eq!(
        fixture
            .store
            .attachment(&fixture.session, "bubblepaw-skills-v1")
            .unwrap(),
        original_skills
    );
    fixture.turn("stable classical prefix after apply").await;
    assert_served_prefix(&fixture, &applied);
    assert_eq!(host_notices(&fixture), delivered_notices);
    fixture.shutdown().await;
    drop(worker);
}
