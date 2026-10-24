use anyhow::Result;

#[test]
fn chapter_rollover_is_an_explicit_optional_policy() {
    let defaults = bubblepaw_runner::episodes::EpisodeConfig::default();
    assert_eq!(defaults.chapter_rollover_tokens, 0);
    defaults.validate().unwrap();
    bubblepaw_runner::episodes::EpisodeConfig {
        chapter_rollover_tokens: 1,
        ..defaults
    }
    .validate()
    .unwrap();
}
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, Harness, HarnessConfig, Message, MessageOrigin, ModelRequest, ModelResponse,
    Provider, Role, RunOptions, Store, TextCallback, ToolCall, ToolContext, ToolRegistry,
    is_typed_user,
};
use bubblepaw_runner::agents::AgentTools;
use bubblepaw_runner::episodes::{
    EpisodeKind, EpisodePlan, apply_split, plan_episodes, turn_summary_cap,
};
use serde_json::json;
use std::sync::Arc;

fn message(id: &str, role: Role, tokens: usize) -> Message {
    serde_json::from_value(json!({
        "id": id,
        "session_id": "session",
        "role": role,
        "content": tokens.to_string(),
        "created_at": "2026-10-08T00:00:00Z"
    }))
    .unwrap()
}

fn user(id: &str) -> Message {
    message(id, Role::User, 0)
}

fn assistant(id: &str, tokens: usize) -> Message {
    message(id, Role::Assistant, tokens)
}

fn plan(history: &[Message], coverage_end: usize) -> EpisodePlan {
    plan_episodes(
        history,
        coverage_end,
        |message| message.content.parse().unwrap(),
        is_typed_user,
    )
    .unwrap()
}

#[test]
fn whole_turn_exact_target_and_unplanned_remainder() {
    let history = vec![
        user("u1"),
        assistant("a1", 24_000),
        user("u2"),
        assistant("a2", 23_999),
        user("u3"),
    ];
    let result = plan(&history, 0);
    assert_eq!(result.episodes.len(), 1);
    let first = &result.episodes[0];
    assert_eq!(first.source_tokens, 24_000);
    assert_eq!(first.kind, EpisodeKind::OpenTurnPart);
    assert_eq!(first.message_range.start, 0);
    assert_eq!(first.message_range.end, 2);
    assert_eq!(first.message_range.first_message_id, "u1");
    assert_eq!(first.message_range.last_message_id, "a1");
    assert_eq!(first.typed_user_message_ids, ["u1"]);
    assert_eq!(result.remainder.unwrap().start, 2);
}

#[test]
fn completed_turn_at_exact_max_is_whole_and_request_tokens_count() {
    let history = vec![
        message("request", Role::User, 1_000),
        assistant("answer", 63_000),
        user("next"),
    ];
    let result = plan(&history, 0);
    assert_eq!(result.episodes.len(), 1);
    assert_eq!(result.episodes[0].source_tokens, 64_000);
    assert_eq!(result.episodes[0].kind, EpisodeKind::OversizedRound);
    assert_eq!(result.episodes[0].message_range.end, 2);
    assert_eq!(result.episodes[0].origin_request_id, None);
}

#[test]
fn multiple_short_turns_pack_at_first_completed_turn_over_target() {
    let history = vec![
        user("u1"),
        assistant("a1", 10_000),
        user("u2"),
        assistant("a2", 10_000),
        user("u3"),
        assistant("a3", 12_000),
        user("u4"),
    ];
    let result = plan(&history, 0);
    assert_eq!(result.episodes.len(), 1);
    assert_eq!(result.episodes[0].source_tokens, 32_000);
    assert_eq!(
        result.episodes[0].typed_user_message_ids,
        ["u1", "u2", "u3"]
    );
    assert_eq!(result.remainder.unwrap().first_message_id, "u4");
}

#[test]
fn complete_rounds_gather_at_target_during_one_open_task() {
    let mut child = user("child");
    child.origin = Some(MessageOrigin::ChildTask);
    let history = vec![
        child,
        assistant("a1", 24_000),
        assistant("a2", 24_000),
        assistant("tail", 1),
    ];
    let result = plan(&history, 0);
    assert_eq!(result.episodes.len(), 2);
    assert_eq!(result.episodes[0].message_range.end, 2);
    assert_eq!(result.episodes[1].message_range.start, 2);
    assert_eq!(
        result.episodes[1].origin_request_id.as_deref(),
        Some("child")
    );
    assert!(
        result
            .episodes
            .iter()
            .all(|e| e.typed_user_message_ids.is_empty())
    );
    assert_eq!(result.remainder.unwrap().first_message_id, "tail");
}

#[test]
fn long_open_turn_has_ordered_complete_parts_and_request_anchors() {
    let history = vec![
        user("u"),
        assistant("a1", 40_000),
        assistant("a2", 30_000),
        assistant("a3", 40_000),
        assistant("a4", 30_000),
    ];
    let result = plan(&history, 0);
    assert_eq!(
        result
            .episodes
            .iter()
            .map(|e| e.source_tokens)
            .collect::<Vec<_>>(),
        [40_000, 30_000, 40_000, 30_000]
    );
    assert_eq!(result.episodes[0].kind, EpisodeKind::OversizedRound);
    assert_eq!(result.episodes[1].kind, EpisodeKind::OpenTurnPart);
    assert_eq!(result.episodes[0].origin_request_id, None);
    assert!(
        result
            .episodes
            .iter()
            .skip(1)
            .all(|e| e.origin_request_id.as_deref() == Some("u"))
    );
    assert!(result.remainder.is_none());
}

#[test]
fn all_out_of_order_tool_results_must_arrive_before_the_round_counts() {
    let mut calls = assistant("calls", 1_000);
    calls.tool_calls = vec![
        ToolCall {
            id: "first".into(),
            name: "tool".into(),
            arguments: json!({}),
        },
        ToolCall {
            id: "second".into(),
            name: "tool".into(),
            arguments: json!({}),
        },
    ];
    let mut second = message("second-result", Role::Tool, 70_000);
    second.tool_call_id = Some("second".into());
    let mut first = message("first-result", Role::Tool, 1);
    first.tool_call_id = Some("first".into());
    let mut history = vec![user("u"), calls, second];
    assert!(plan(&history, 0).episodes.is_empty());
    history.push(first);
    let result = plan(&history, 0);
    assert_eq!(result.episodes.len(), 1);
    assert_eq!(result.episodes[0].source_tokens, 71_001);
    assert_eq!(result.episodes[0].kind, EpisodeKind::OversizedRound);
    assert_eq!(
        result.episodes[0].message_range.last_message_id,
        "first-result"
    );
    assert!(result.remainder.is_none());
}

#[test]
fn oversized_round_stands_alone_after_a_target_sized_pack() {
    for prefix_tokens in [32_000, 40_000] {
        let history = vec![
            user("u"),
            assistant("a1", prefix_tokens),
            assistant("huge", 70_000),
            assistant("tail", 5),
        ];
        let result = plan(&history, 0);
        assert_eq!(result.episodes.len(), 2);
        assert_eq!(result.episodes[0].source_tokens, prefix_tokens);
        assert_eq!(
            result.episodes[0].kind,
            if prefix_tokens > 32_000 {
                EpisodeKind::OversizedRound
            } else {
                EpisodeKind::OpenTurnPart
            }
        );
        assert_eq!(result.episodes[1].source_tokens, 70_000);
        assert_eq!(result.episodes[1].kind, EpisodeKind::OversizedRound);
        assert_eq!(result.episodes[1].message_range.first_message_id, "huge");
        assert_eq!(result.episodes[1].origin_request_id.as_deref(), Some("u"));
    }
}

#[test]
fn maximum_prefers_previous_complete_boundary_but_never_splits_a_large_round() {
    for round_tokens in [55_000, 70_000] {
        let history = vec![
            user("u"),
            assistant("small", 10_000),
            assistant("large", round_tokens),
            user("next"),
        ];
        let result = plan(&history, 0);
        assert_eq!(result.episodes.len(), 2);
        assert_eq!(result.episodes[0].source_tokens, 10_000);
        assert_eq!(result.episodes[1].source_tokens, round_tokens);
        assert_eq!(result.episodes[1].kind, EpisodeKind::OversizedRound);
        assert_eq!(result.episodes[1].message_range.end, 3);
    }
}

#[test]
fn configuration_controls_source_targets_and_rejects_only_structural_invalidity() {
    use bubblepaw_runner::episodes::{EpisodeConfig, plan_episodes_with_config};
    let history = vec![
        user("u"),
        assistant("a", 16),
        assistant("b", 16),
        assistant("c", 16),
    ];
    let config = EpisodeConfig {
        source_target_tokens: 32,
        source_max_tokens: 40,
        ..Default::default()
    };
    let result = plan_episodes_with_config(
        &history,
        0,
        |m| m.content.parse().unwrap(),
        is_typed_user,
        &config,
    )
    .unwrap();
    assert_eq!(result.episodes[0].source_tokens, 32);
    assert_eq!(result.remainder.unwrap().first_message_id, "c");
    assert!(
        EpisodeConfig {
            source_target_tokens: 1,
            source_max_tokens: 1,
            summary_target_tokens: 1,
            chapter_summary_max_tokens: 1,
            chapter_description_tokens: 1,
            chapter_rollover_tokens: 1,
            tool_evidence_tokens: 1,
            decision_context_tokens: 1,
            ..Default::default()
        }
        .validate()
        .is_ok()
    );
    assert!(
        EpisodeConfig {
            source_max_tokens: 1,
            ..Default::default()
        }
        .validate()
        .is_err()
    );
}

#[test]
fn split_at_first_or_interior_user_has_exact_tokens_and_replans_rest() {
    let history = vec![
        user("u1"),
        assistant("a1", 10_000),
        user("u2"),
        assistant("a2", 22_000),
        user("u3"),
        assistant("a3", 10_000),
        user("u4"),
    ];
    let result = plan(&history, 0);
    let item = &result.episodes[0];
    let first = apply_split(item, "u1").unwrap();
    assert!(first.before.is_none());
    assert_eq!(first.remaining.start, 0);
    assert_eq!(first.remaining.end, 4);
    let interior = apply_split(item, "u2").unwrap();
    let before = interior.before.unwrap();
    assert_eq!(before.source_tokens, 10_000);
    assert_eq!(before.message_range.end, 2);
    assert_eq!(before.typed_user_message_ids, ["u1"]);
    assert_eq!(interior.remaining.first_message_id, "u2");
    let replanned = plan(&history, interior.remaining.start);
    assert_eq!(replanned.episodes[0].source_tokens, 32_000);
    assert_eq!(replanned.episodes[0].typed_user_message_ids, ["u2", "u3"]);
    assert!(apply_split(item, "a1").is_err());
    assert!(apply_split(item, "missing").is_err());
}

#[test]
fn covered_request_is_found_backwards_and_legacy_missing_request_is_none() {
    let history = vec![
        user("old"),
        assistant("old-answer", 0),
        user("request"),
        assistant("covered", 10),
        assistant("part", 40_000),
        assistant("next-round", 30_000),
    ];
    let result = plan(&history, 4);
    assert_eq!(result.episodes[0].message_range.start, 4);
    assert_eq!(
        result.episodes[0].origin_request_id.as_deref(),
        Some("request")
    );
    assert!(result.episodes[0].typed_user_message_ids.is_empty());
    let legacy = vec![
        assistant("covered", 10),
        assistant("part", 40_000),
        assistant("next-round", 30_000),
    ];
    assert_eq!(plan(&legacy, 1).episodes[0].origin_request_id, None);
    assert!(plan(&history, history.len()).episodes.is_empty());
}

#[test]
fn pending_calls_before_coverage_still_need_all_results() {
    let mut calls = assistant("covered-call", 1);
    calls.tool_calls = vec![ToolCall {
        id: "call".into(),
        name: "tool".into(),
        arguments: json!({}),
    }];
    let mut output = message("output", Role::Tool, 70_000);
    output.tool_call_id = Some("call".into());
    let history = vec![user("request"), calls, output];
    let result = plan(&history, 2);
    assert_eq!(result.episodes[0].source_tokens, 70_000);
    assert_eq!(
        result.episodes[0].origin_request_id.as_deref(),
        Some("request")
    );
}

#[test]
fn caller_origin_predicate_controls_turn_boundaries_and_split_options() {
    let history = vec![
        user("typed"),
        assistant("a1", 20_000),
        user("generated"),
        assistant("a2", 20_000),
        user("next"),
    ];
    let result = plan_episodes(
        &history,
        0,
        |message| message.content.parse().unwrap(),
        |message| message.role == Role::User && message.id != "generated",
    )
    .unwrap();
    assert_eq!(result.episodes[0].typed_user_message_ids, ["typed"]);
    assert!(apply_split(&result.episodes[0], "generated").is_err());
}

#[test]
fn invalid_coverage_and_token_overflow_return_errors() {
    assert!(plan_episodes(&[], 1, |_| 0, is_typed_user).is_err());
    let history = vec![user("u"), assistant("a", 0)];
    assert!(plan_episodes(&history, 0, |_| usize::MAX, is_typed_user).is_err());
}

#[test]
fn summary_cap_rounds_ties_up_with_a_minimum_and_without_overflow() {
    for (source, cap) in [
        (0, 100),
        (799, 100),
        (800, 100),
        (1_599, 100),
        (2_400, 200),
        (32_000, 2_000),
        (32_800, 2_100),
        (32_801, 2_100),
        (33_599, 2_100),
        (64_000, 4_000),
    ] {
        assert_eq!(turn_summary_cap(source), cap, "source tokens: {source}");
    }
    let expected_max = (((usize::MAX as u128 + 800) / 1_600) * 100) as usize;
    assert_eq!(turn_summary_cap(usize::MAX), expected_max);
}

struct ImmediateProvider;

#[async_trait]
impl Provider for ImmediateProvider {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        Ok(ModelResponse {
            text: "completed".into(),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn actual_child_spawn_marks_unwrapped_and_wrapped_prompts_as_child_task() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(root.path().join("archive.sqlite")).unwrap();
    let child = Harness::new(
        store.clone(),
        Arc::new(ImmediateProvider),
        ToolRegistry::new(),
        HarnessConfig::default(),
    );
    let manager = AgentTools::new(store.clone(), child, None);
    let parent = store.create_session("parent").unwrap();
    let owner = ToolContext {
        session_id: parent.id,
        run_id: "parent-run".into(),
        invocation_id: "spawn-invocation".into(),
        cancellation: CancellationToken::new(),
        approval: Default::default(),
    };
    for input in [
        json!({"task":"plain child"}),
        json!({"task":"wrapped child", "context":{"note":"supplied context"}}),
    ] {
        let started = manager.start(&owner, input.clone()).unwrap();
        let result = manager
            .wait(
                &owner.session_id,
                started["handle"].as_str().unwrap(),
                Some(2_000),
            )
            .await
            .unwrap();
        assert_eq!(result["status"], "completed");
        let history = store
            .messages(started["sessionId"].as_str().unwrap())
            .unwrap();
        let prompt = &history[0];
        assert_eq!(prompt.role, Role::User);
        assert_eq!(prompt.origin, Some(MessageOrigin::ChildTask));
        assert!(!is_typed_user(prompt));
        if input.get("context").is_some() {
            assert!(prompt.content.starts_with("Task:\nwrapped child"));
        } else {
            assert_eq!(prompt.content, "plain child");
        }
    }
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn attachment_envelope_preserves_generated_or_typed_submission_origin() {
    use bubblepaw_runner::input_files::{InputFiles, InputLimits};

    let root = tempfile::tempdir().unwrap();
    let store = Store::open(root.path().join("archive.sqlite")).unwrap();
    let input_files = InputFiles::new(root.path(), InputLimits::default()).unwrap();
    let harness = Harness::new(
        store.clone(),
        Arc::new(ImmediateProvider),
        ToolRegistry::new(),
        HarnessConfig::default(),
    );
    for origin in [MessageOrigin::Generated, MessageOrigin::Typed] {
        let session = store.create_session("attachment").unwrap();
        let file = input_files
            .upload(&session.id, "note.txt", b"attached text")
            .unwrap();
        let envelope = input_files.prepare(&session.id, "", &[file.id]).unwrap();
        assert!(envelope.starts_with("Attached files."));
        harness
            .prompt(
                &session.id,
                envelope,
                RunOptions {
                    origin,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let history = store.messages(&session.id).unwrap();
        assert_eq!(history[0].origin, Some(origin));
        assert_eq!(is_typed_user(&history[0]), origin == MessageOrigin::Typed);
    }
}
