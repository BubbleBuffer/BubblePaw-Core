mod support;

use bubblepaw_core::{
    ContextClassification as Class, ContextState, MessageOrigin, ModelRequest, ReasoningEffort,
    Reminder, Role, ToolDefinition,
};
use serde_json::json;
use support::unstamped_message;

fn parent(contents: &[&str]) -> ModelRequest {
    ModelRequest {
        model: "route/main".into(),
        system_prompt: "system".into(),
        tools: vec![ToolDefinition {
            name: "read".into(),
            description: "read".into(),
            input_schema: json!({"type":"object"}),
            read_only: true,
            approval: Default::default(),
        }],
        messages: contents
            .iter()
            .map(|s| unstamped_message("s", "id", Role::User, s))
            .collect(),
        ..ModelRequest::default()
    }
}
fn wire(request: &ModelRequest) -> serde_json::Value {
    json!({"system":request.system_prompt,"tools":request.tools.iter().map(|t| &t.name).collect::<Vec<_>>(),
        "messages":request.messages.iter().map(|m| json!([m.role,m.content])).collect::<Vec<_>>()})
}

#[test]
fn fork_sends_the_parent_prefix_then_the_task_reminder_and_reads_its_cache() {
    let state = ContextState::default();
    state.build(parent(&["cold", "a"]), "s", "root", &[0]);
    let last = parent(&["cold", "a", "b", "c"]);
    let built = state.build(last.clone(), "s", "root", &[0]);
    assert_eq!(built.tiers.previous_len, Some(2));

    let forked = state.fork(
        last.clone(),
        "s",
        "root",
        last.messages.len(),
        "chapter",
        &[0],
        Some(Reminder::new("chapter_task", "Close the chapter in text.")),
    );
    let (sent, own) = forked.request.messages.split_at(last.messages.len());
    assert_eq!(
        wire(&ModelRequest {
            messages: sent.to_vec(),
            ..forked.request.clone()
        }),
        wire(&last)
    );
    assert_eq!(forked.request.system_prompt, last.system_prompt);
    assert_eq!(forked.request.tools.len(), last.tools.len());
    assert_eq!(own.len(), 1);
    assert_eq!(own[0].role, Role::User);
    assert_eq!(own[0].origin, Some(MessageOrigin::Generated));
    assert_eq!(
        own[0].content,
        "<harness kind=\"chapter_task\">Close the chapter in text.</harness>"
    );
    assert_eq!(forked.record.classification, Class::Append);
    assert!(!forked.record.declared);
    // The fork marks the parent's last block, where the parent wrote its cache.
    assert_eq!(forked.tiers.previous_len, Some(4));
    assert_eq!(forked.tiers.cold_end, built.tiers.cold_end);
}

#[test]
fn fork_cut_or_extended_is_an_append_and_leaves_the_parent_lineage_unchanged() {
    let state = ContextState::default();
    let last = parent(&["a", "b", "c"]);
    state.build(last.clone(), "s", "root", &[]);

    // Cut before the parent's end: the fork point is marked.
    let mut cut = last.clone();
    cut.messages.truncate(2);
    cut.messages
        .push(unstamped_message("s", "id", Role::User, "summary control"));
    let built = state.fork(cut, "s", "root", 2, "summary_fork", &[], None);
    assert_eq!(built.record.classification, Class::Append);
    assert_eq!(built.tiers.previous_len, Some(2));

    // Extended with committed rows past the parent's end.
    let mut extended = last.clone();
    extended
        .messages
        .push(unstamped_message("s", "id", Role::User, "d"));
    let built = state.fork(extended, "s", "root", 4, "summary_fork", &[], None);
    assert_eq!(built.record.classification, Class::Append);
    assert_eq!(built.tiers.previous_len, Some(3));

    // Neither fork replaced the parent's snapshot or recorded a fork lineage.
    let next = state.build(parent(&["a", "b", "c", "d"]), "s", "root", &[]);
    assert_eq!(next.record.classification, Class::Append);
    assert_eq!(next.tiers.previous_len, Some(3));
    let first = state.build(parent(&["x"]), "s", "summary_fork", &[]);
    assert_eq!(first.record.classification, Class::First);
}

#[test]
fn fork_with_a_different_prefix_is_not_an_append() {
    let state = ContextState::default();
    state.build(parent(&["a", "b"]), "s", "root", &[]);
    let built = state.fork(parent(&["a", "x"]), "s", "root", 2, "chapter", &[], None);
    assert_eq!(
        built.record.classification,
        Class::HistoryRewrite {
            first_differing_index: 1
        }
    );
    assert_eq!(built.tiers.previous_len, None);
    let mut other = parent(&["a", "b"]);
    other.system_prompt = "chapter system".into();
    let built = state.fork(other, "s", "root", 2, "chapter", &[], None);
    assert_eq!(built.record.classification, Class::FrozenChange);
    let built = state.fork(parent(&["a"]), "other", "root", 1, "chapter", &[], None);
    assert_eq!(built.record.classification, Class::First);
}

#[test]
fn fresh_side_calls_declare_why_they_are_not_forks() {
    let state = ContextState::default();
    let reason = |request: ModelRequest, kind: &str| {
        let built = state.build_side(request, "s", kind, &[], "root", None);
        assert!(built.record.declared);
        built.record.declaration_reason.unwrap()
    };
    assert_eq!(
        reason(parent(&["task"]), "decider"),
        ContextState::SIDE_CALL_NO_CAPTURE
    );
    state.build(parent(&["a"]), "s", "root", &[]);
    let mut other = parent(&["task"]);
    other.model = "route/luna".into();
    assert_eq!(
        reason(other, "decider"),
        ContextState::SIDE_CALL_OTHER_MODEL
    );
    let mut low = parent(&["task"]);
    low.reasoning_effort = Some(ReasoningEffort::Low);
    assert_eq!(reason(low, "decider"), ContextState::SIDE_CALL_SETTINGS);
    let mut capped = parent(&["task"]);
    capped.max_output_tokens = Some(64);
    assert_eq!(reason(capped, "chapter"), ContextState::SIDE_CALL_SETTINGS);
    assert_eq!(
        reason(parent(&["task"]), "summary"),
        ContextState::SIDE_CALL_UNFORKED
    );
    let explicit = state.build_side(
        parent(&["task"]),
        "s",
        "summary",
        &[],
        "root",
        Some(ContextState::SIDE_CALL_EPISODE_INPUT),
    );
    assert_eq!(
        explicit.record.declaration_reason.as_deref(),
        Some(ContextState::SIDE_CALL_EPISODE_INPUT)
    );
}

#[test]
fn fork_uses_the_parent_cold_split_whatever_the_caller_annotated() {
    let state = ContextState::default();
    let last = parent(&["cold", "notes", "a"]);
    state.build(last.clone(), "s", "root", &[0, 1]);
    // The capture knew only the first cold message; the bytes are the same.
    let built = state.fork(last, "s", "root", 3, "summary_fork", &[0], None);
    assert_eq!(built.record.classification, Class::Append);
    assert_eq!(built.record.cold.message_count, 2);
    assert_eq!(built.tiers.cold_end, Some(2));
}
