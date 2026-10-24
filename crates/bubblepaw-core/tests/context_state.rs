mod support;

use bubblepaw_core::{
    CacheAccounting, ContextClassification as Class, ContextState, ModelRequest, Observation,
    ObservationMode, ObservationRecord, ObservationScope, ObservationSink, RequestKind, Role,
    ToolDefinition, Usage, WireShape,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use support::unstamped_message;

fn request(contents: &[&str]) -> ModelRequest {
    ModelRequest {
        system_prompt: "system".into(),
        messages: contents
            .iter()
            .map(|s| unstamped_message("session", "id", Role::User, s))
            .collect(),
        ..ModelRequest::default()
    }
}
fn tool(schema: Value) -> ToolDefinition {
    ToolDefinition {
        name: "tool".into(),
        description: "description".into(),
        input_schema: schema,
        read_only: false,
        approval: Default::default(),
    }
}

#[test]
fn classifies_tier_changes_and_history_divergence() {
    let state = ContextState::default();
    let build = |r, indexes: &[usize]| {
        let record = state.build(r, "s", "root", indexes).record;
        assert!(!record.declared);
        record
    };
    assert_eq!(
        build(request(&["cold", "a"]), &[0]).classification,
        Class::First
    );
    assert_eq!(
        build(request(&["cold", "a", "b"]), &[0]).classification,
        Class::Append
    );
    assert_eq!(
        build(request(&["cold", "a", "b"]), &[0]).classification,
        Class::Append
    );
    assert_eq!(
        build(request(&["cold", "a", "x"]), &[0]).classification,
        Class::HistoryRewrite {
            first_differing_index: 1
        }
    );
    assert_eq!(
        build(request(&["cold", "a"]), &[0]).classification,
        Class::HistoryRewrite {
            first_differing_index: 1
        }
    );
    assert_eq!(
        build(request(&["new cold", "a"]), &[0]).classification,
        Class::ColdChange
    );
    let mut changed = request(&["new cold", "a"]);
    changed.system_prompt.push('!');
    assert_eq!(build(changed, &[0]).classification, Class::FrozenChange);
}

#[test]
fn frozen_contains_declared_tools_but_excludes_generation_and_route() {
    let state = ContextState::default();
    state.build(request(&["a"]), "s", "root", &[]);
    let mut r = request(&["a"]);
    r.tools.push(tool(json!({"type":"object"})));
    assert_eq!(
        state
            .build(r.clone(), "s", "root", &[])
            .record
            .classification,
        Class::FrozenChange
    );
    // Moving the declarations to disabled_tools enforces tools-off: a frozen change.
    r.disabled_tools.push(r.tools.remove(0));
    assert_eq!(
        state
            .build(r.clone(), "s", "root", &[])
            .record
            .classification,
        Class::FrozenChange
    );
    r.model = "another route".into();
    r.max_output_tokens = Some(123);
    r.reasoning_effort = Some(bubblepaw_core::ReasoningEffort::High);
    r.output_schema = Some(
        bubblepaw_core::JsonSchema::new(
            "x",
            json!({"type":"object","properties":{},"additionalProperties":false}),
        )
        .unwrap(),
    );
    assert_eq!(
        state.build(r, "s", "root", &[]).record.classification,
        Class::Append
    );
}

#[test]
fn explicit_declarations_cover_frozen_cold_and_history_changes_once() {
    let state = ContextState::default();
    state.build(request(&["cold", "a"]), "s", "root", &[0]);
    state.declare(Some("s"), "tools_reloaded");
    let mut r = request(&["cold", "a"]);
    r.system_prompt.push('!');
    let frozen = state.build(r.clone(), "s", "root", &[0]).record;
    assert_eq!(frozen.classification, Class::FrozenChange);
    assert!(frozen.declared);
    assert_eq!(frozen.declaration_reason.as_deref(), Some("tools_reloaded"));
    state.declare(Some("s"), "lcm_apply");
    r.messages[0].content = "new cold".into();
    let cold = state.build(r.clone(), "s", "root", &[0]).record;
    assert_eq!(cold.classification, Class::ColdChange);
    assert!(cold.declared);
    state.declare(None, "rewind");
    r.messages[1].content = "rewritten".into();
    let history = state.build(r.clone(), "s", "root", &[0]).record;
    assert_eq!(
        history.classification,
        Class::HistoryRewrite {
            first_differing_index: 0
        }
    );
    assert!(history.declared);
    r.messages[1].content = "undeclared second rewrite".into();
    assert!(!state.build(r, "s", "root", &[0]).record.declared);
}

#[test]
fn explicit_cold_indexes_preserve_request_content_and_order() {
    let state = ContextState::default();
    let r = request(&[
        "<compaction_view>looks like a marker</compaction_view>",
        "ordinary",
    ]);
    let before = serde_json::to_vec(&r).unwrap();
    let built = state.build(r, "s", "root", &[]);
    assert_eq!(serde_json::to_vec(&built.request).unwrap(), before);
    assert_eq!(built.record.cold.message_count, 0);
    assert_eq!(built.record.history.message_count, 2);
    let marked = state.build(built.request, "s", "root", &[0, 0, 99]);
    assert_eq!(marked.record.classification, Class::ColdChange);
    assert_eq!(marked.record.cold.message_count, 1);
    assert_eq!(marked.record.history.message_count, 1);
    assert_eq!(marked.record.invalid_cold_indexes, 1);
    assert_eq!(serde_json::to_vec(&marked.request).unwrap(), before);
}

#[test]
fn lineages_are_session_and_request_kind_and_can_be_released() {
    let state = ContextState::default();
    let build = |session, kind, r| state.build(r, session, kind, &[]).record.classification;
    assert_eq!(build("s", "root", request(&["a"])), Class::First);
    assert_eq!(
        build("s", "summary_fork", request(&["other"])),
        Class::First
    );
    assert_eq!(
        build("child", "root", request(&["child input"])),
        Class::First
    );
    assert_eq!(build("s", "root", request(&["a", "b"])), Class::Append);
    state.release_session("s");
    assert_eq!(build("s", "root", request(&["a"])), Class::First);
    assert_eq!(
        build("s", "summary_fork", request(&["other"])),
        Class::First
    );
    assert_eq!(
        build("child", "root", request(&["child input", "more"])),
        Class::Append
    );
}

#[test]
fn canonical_content_ignores_metadata_and_sorts_nested_object_keys() {
    let state = ContextState::default();
    let mut a = request(&["a"]);
    a.tools.push(tool(
        serde_json::from_str(r#"{"z":{"b":2,"a":1},"a":0}"#).unwrap(),
    ));
    a.messages[0].provider_state =
        Some(serde_json::from_str(r#"{"z":2,"a":{"b":2,"a":1}}"#).unwrap());
    let first = state.build(a.clone(), "s", "root", &[]).record;
    let mut b = a;
    b.tools[0].input_schema = serde_json::from_str(r#"{"a":0,"z":{"a":1,"b":2}}"#).unwrap();
    b.messages[0].provider_state =
        Some(serde_json::from_str(r#"{"a":{"a":1,"b":2},"z":2}"#).unwrap());
    b.messages[0].id = "new id".into();
    b.messages[0].created_at = "new timestamp".into();
    b.messages[0].session_id = "new metadata".into();
    b.messages[0].origin = Some(bubblepaw_core::MessageOrigin::Generated);
    b.tools[0].read_only = true;
    let second = state.build(b.clone(), "s", "root", &[]).record;
    assert_eq!(first.frozen, second.frozen);
    assert_eq!(first.history, second.history);
    assert_eq!(second.classification, Class::Append);
    b.messages[0].provider_state = Some(json!({"changed":"signed replay"}));
    assert_eq!(
        state.build(b, "s", "root", &[]).record.classification,
        Class::HistoryRewrite {
            first_differing_index: 0
        }
    );
}

#[test]
fn declarations_reach_context_without_consuming_legacy_cache_declarations() {
    let cache = CacheAccounting::default();
    cache.context.build(request(&["a"]), "s", "root", &[]);
    cache.context.build(request(&["a"]), "other", "root", &[]);
    let finish = |id, content| {
        let shape = WireShape {
            provider: "p".into(),
            model: "m".into(),
            ..WireShape::from_body(&json!({"messages":[content]}), "messages", 64)
        };
        cache.start(id, "s", RequestKind::Main, shape, vec![]);
        cache
            .finish(id, Some(&Usage::default()), None, true)
            .unwrap()
    };
    finish("first", "a");
    cache.declare(Some("s"), "lcm_apply");
    let changed = cache
        .context
        .build(request(&["b"]), "s", "root", &[])
        .record;
    assert!(changed.declared);
    assert_eq!(changed.declaration_reason.as_deref(), Some("lcm_apply"));
    assert_eq!(
        finish("changed", "b").prefix_break.unwrap().reason,
        "lcm_apply"
    );
    assert!(
        !cache
            .context
            .build(request(&["b"]), "other", "root", &[])
            .record
            .declared
    );
    assert!(
        !cache
            .context
            .build(request(&["c"]), "s", "root", &[])
            .record
            .declared
    );
    cache.declare(None, "reload");
    assert!(
        cache
            .context
            .build(request(&["d"]), "s", "root", &[])
            .record
            .declared
    );
    assert!(
        cache
            .context
            .build(request(&["c"]), "other", "root", &[])
            .record
            .declared
    );
    // Legacy finishing first also cannot consume the tier declaration.
    cache.declare(Some("s"), "again");
    finish("legacy first", "c");
    assert!(
        cache
            .context
            .build(request(&["e"]), "s", "root", &[])
            .record
            .declared
    );
}

#[derive(Default)]
struct Sink(Mutex<Vec<ObservationRecord>>);
impl ObservationSink for Sink {
    fn record(&self, record: ObservationRecord) {
        self.0.lock().unwrap().push(record);
    }
}
#[test]
fn context_observation_contains_only_metadata_in_contents_mode() {
    let sink = Arc::new(Sink::default());
    let observer = Observation {
        sink: sink.clone(),
        mode: ObservationMode::Contents,
        max_payload_bytes: 1000,
    };
    let built = ContextState::default().build(request(&["private content"]), "s", "root", &[]);
    observer.context_built(
        ObservationScope {
            session_id: "s".into(),
            ..Default::default()
        },
        &built.record,
    );
    let records = sink.0.lock().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, "context_built");
    assert!(records[0].payload.is_none());
    assert!(!records[0].metrics.to_string().contains("private content"));
    assert_eq!(records[0].metrics["classification"]["kind"], "first");
}

#[test]
fn a_changed_declaration_is_a_frozen_change_even_when_moved_to_disabled() {
    let state = ContextState::default();
    let mut r = request(&["a"]);
    r.tools.push(tool(json!({"type":"object"})));
    state.build(r.clone(), "s", "root", &[]);
    r.disabled_tools = std::mem::take(&mut r.tools);
    assert_eq!(
        state
            .build(r.clone(), "s", "root", &[])
            .record
            .classification,
        Class::FrozenChange,
        "the enforced move is a frozen change"
    );
    assert_eq!(
        state
            .build(r.clone(), "s", "root", &[])
            .record
            .classification,
        Class::Append,
        "the same disabled set again is an append"
    );
    r.disabled_tools[0].description.push('!');
    assert_eq!(
        state.build(r, "s", "root", &[]).record.classification,
        Class::FrozenChange
    );
}
