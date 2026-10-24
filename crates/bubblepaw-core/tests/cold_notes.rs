//! Cold notes render after the view, in a fixed order, and change only at a
//! declared change, so the cold tier stays byte-stable between applies.
mod support;

use bubblepaw_core::{
    ContextClassification as Class, ContextState, MessageOrigin, ModelRequest, Role,
};
use support::unstamped_message;

fn request(contents: &[&str]) -> ModelRequest {
    ModelRequest {
        system_prompt: "system".into(),
        messages: contents
            .iter()
            .map(|s| unstamped_message("s", s, Role::System, s))
            .collect(),
        ..ModelRequest::default()
    }
}
/// Place the notes, then classify, as build_persisted does.
fn build(
    state: &ContextState,
    contents: &[&str],
    cold: &[usize],
) -> (ModelRequest, Class, Vec<usize>) {
    let mut request = request(contents);
    let cold = state.place_cold_notes(&mut request, "s", "root", cold);
    let built = state.build(request, "s", "root", &cold);
    (built.request, built.record.classification, cold)
}

#[test]
fn notes_render_after_the_cold_tier_by_kind_then_insertion() {
    let state = ContextState::default();
    state.cold_note("s", "b", "first b");
    state.cold_note("s", "a", "only a");
    state.cold_note("s", "b", "second b");
    state.cold_note("s", "b", "first b");
    let (request, class, cold) = build(&state, &["view 1", "view 2", "raw"], &[0, 1]);
    assert_eq!(class, Class::First);
    assert_eq!(
        cold,
        vec![0, 1, 2],
        "the note message belongs to the cold tier"
    );
    let contents: Vec<_> = request
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect();
    assert_eq!(contents[..2], ["view 1", "view 2"]);
    assert_eq!(contents[3], "raw");
    assert_eq!(
        contents[2],
        "<harness kind=\"a\">only a</harness>\n<harness kind=\"b\">first b</harness>\n<harness kind=\"b\">second b</harness>"
    );
    let note = &request.messages[2];
    assert_eq!(note.role, Role::System);
    assert_eq!(note.origin, Some(MessageOrigin::Generated));
    assert_eq!(
        note.created_at, "time",
        "timestamp of the last view message"
    );
    assert!(note.id.starts_with("context:cold_notes:"));
}

#[test]
fn notes_change_only_at_a_declared_change() {
    let state = ContextState::default();
    state.cold_note("s", "history_lookup", "lookup v1");
    let (first, _, _) = build(&state, &["view", "a"], &[0]);
    state.clear_cold_notes("s");
    state.cold_note("s", "history_lookup", "lookup v2");
    let (second, class, _) = build(&state, &["view", "a", "b"], &[0]);
    assert_eq!(
        class,
        Class::Append,
        "an undeclared note change is held back"
    );
    assert_eq!(second.messages[..3], first.messages[..3]);
    assert!(second.messages[1].content.contains("lookup v1"));
    state.declare(Some("s"), "lcm_apply");
    let (third, class, _) = build(&state, &["new view", "b"], &[0]);
    assert_eq!(class, Class::ColdChange);
    assert!(third.messages[1].content.contains("lookup v2"));
    let (fourth, class, _) = build(&state, &["new view", "b", "c"], &[0]);
    assert_eq!(class, Class::Append);
    assert_eq!(
        fourth.messages[1], third.messages[1],
        "byte-stable after the apply"
    );
}

#[test]
fn a_request_without_a_cold_tier_carries_no_notes() {
    let state = ContextState::default();
    state.cold_note("s", "history_lookup", "lookup");
    let (request, _, cold) = build(&state, &["a", "b"], &[]);
    assert!(cold.is_empty());
    assert_eq!(request.messages.len(), 2);
    state.release_session("s");
    let (request, _, _) = build(&state, &["view", "a"], &[0]);
    assert_eq!(
        request.messages.len(),
        2,
        "released sessions keep no staged notes"
    );
}

#[test]
fn tiers_report_the_cold_end_and_the_appended_previous_length() {
    let state = ContextState::default();
    state.cold_note("s", "history_lookup", "lookup");
    let tiers = |contents: &[&str], cold: &[usize]| {
        let mut request = request(contents);
        let cold = state.place_cold_notes(&mut request, "s", "root", cold);
        state.build(request, "s", "root", &cold).tiers
    };
    let first = tiers(&["view", "a"], &[0]);
    assert_eq!((first.cold_end, first.previous_len), (Some(2), None));
    let append = tiers(&["view", "a", "b", "c"], &[0]);
    assert_eq!((append.cold_end, append.previous_len), (Some(2), Some(3)));
    state.declare(Some("s"), "lcm_apply");
    let apply = tiers(&["new view", "c"], &[0]);
    assert_eq!((apply.cold_end, apply.previous_len), (Some(2), None));
    let plain = ContextState::default().build(request(&["a"]), "s", "root", &[]);
    assert_eq!(plain.tiers, bubblepaw_core::CacheTiers::default());
}
