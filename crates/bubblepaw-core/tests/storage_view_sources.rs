mod support;

use anyhow::Result;
use bubblepaw_core::{ContextView, Message, Role, Store};
use rusqlite::{Connection, params};
use serde_json::json;
use support::stored_message;

const DATE: &str = "2026-10-12T00:00:00Z";
fn view(session: &str, prefix: Vec<Message>) -> ContextView {
    ContextView {
        session_id: session.into(),
        frontier: None,
        system_prompt: "frozen".into(),
        prefix,
        masked_tool_output_ids: vec![],
        clipped_tool_call_ids: vec![],
        created_at: DATE.into(),
        threshold: 200,
        budget: 100,
        tokens: 20,
        blocks: Vec::new(),
        blocks_estimated: true,
    }
}
fn unreadable(store: &Store, session: &str, id: &str) -> Result<()> {
    Connection::open(store.chat_path(session)?.unwrap())?.execute(
        "UPDATE messages SET data=?2 WHERE message_id=?1",
        params![
            id,
            json!({"_row_codec":"zstd-dict-v1","dictionary":"missing","raw_bytes":10000,"data":""})
                .to_string()
        ],
    )?;
    Ok(())
}
fn stored_view(dir: &std::path::Path) -> Result<String> {
    Ok(Connection::open(dir.join("serving.db"))?.query_row(
        "SELECT data FROM context_views",
        [],
        |row| row.get(0),
    )?)
}

#[test]
fn saving_embedded_typed_user_anchor_ignores_unrelated_cold_bodies() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("selected provenance")?;
    let source = stored_message(
        &session.id,
        "selected-user",
        Role::User,
        "Only this original typed request with Unicode 猫 is needed.",
    );
    store.append_batch(&[
        stored_message(
            &session.id,
            "unrelated-cold",
            Role::Tool,
            &"irrelevant output ".repeat(10000),
        ),
        stored_message(
            &session.id,
            "older-user",
            Role::User,
            "older irrelevant request",
        ),
        source.clone(),
    ])?;
    unreadable(&store, &session.id, "unrelated-cold")?;
    unreadable(&store, &session.id, "older-user")?;
    let rendered = stored_message(
        &session.id,
        "synthetic-user-block",
        Role::System,
        &format!(
            "[{} | {}]\n{}\nOlder typed user message IDs (history_read):\nolder-user\nunrelated-cold",
            source.id, source.created_at, source.content
        ),
    );
    let captured = view(&session.id, vec![rendered]);
    store.save_context_view(&captured)?;
    store.flush_serving()?;
    let data = stored_view(dir.path())?;
    assert!(!data.contains(&source.content));
    let encoded: serde_json::Value = serde_json::from_str(&data)?;
    assert!(
        encoded["prefix_refs"][0]["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|part| part["kind"] == "source" && part["table"] == "messages")
    );
    assert_eq!(
        serde_json::to_vec(&store.context_view(&session.id)?.unwrap())?,
        serde_json::to_vec(&captured)?
    );
    Ok(())
}

#[test]
fn canonical_prefix_ids_preserve_clipped_content_and_provider_bytes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("same-id projection")?;
    let mut source = stored_message(
        &session.id,
        "selected-assistant",
        Role::Assistant,
        "Canonical assistant text with enough words to use byte references.",
    );
    source.tool_calls = vec![bubblepaw_core::ToolCall {
        id: "call".into(),
        name: "test".into(),
        arguments: json!({"value":0.0,"path":"original path"}),
    }];
    source.provider_state = Some(
        json!({"opaque":"signed bytes","adapter":"responses","items":[{"type":"function_call","call_id":"call","arguments":"{ \"value\" : 0.0 }"}]}),
    );
    store.append_batch(&[
        stored_message(
            &session.id,
            "cold-output",
            Role::Tool,
            &"unused ".repeat(10000),
        ),
        source.clone(),
    ])?;
    unreadable(&store, &session.id, "cold-output")?;
    let mut selected = source.clone();
    selected.content = format!("Clipped: {}", &source.content[10..]);
    selected.tool_calls[0].arguments = json!({"value":-0.0,"path":"original path"});
    let captured = view(&session.id, vec![selected]);
    store.save_context_view(&captured)?;
    store.flush_serving()?;
    assert_eq!(
        serde_json::to_vec(&store.context_view(&session.id)?.unwrap())?,
        serde_json::to_vec(&captured)?
    );
    assert!(!stored_view(dir.path())?.contains("assistant text with enough words"));
    Ok(())
}

#[test]
fn anonymous_custom_template_does_not_search_original_bodies() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("anonymous template")?;
    let text = "Unattributed custom prefix text with no canonical source identity.";
    store.append_batch(&[stored_message(
        &session.id,
        "unselected-original",
        Role::User,
        text,
    )])?;
    unreadable(&store, &session.id, "unselected-original")?;
    let captured = view(
        &session.id,
        vec![stored_message(&session.id, "anonymous", Role::System, text)],
    );
    store.save_context_view(&captured)?;
    store.flush_serving()?;
    assert_eq!(
        serde_json::to_vec(&store.context_view(&session.id)?.unwrap())?,
        serde_json::to_vec(&captured)?
    );
    let encoded: serde_json::Value = serde_json::from_str(&stored_view(dir.path())?)?;
    assert_eq!(encoded["prefix_refs"][0]["content"][0]["kind"], "literal");
    Ok(())
}

#[test]
fn later_repeated_source_occurrence_keeps_the_full_correct_excerpt_as_a_pointer() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("divergent repeated prefix")?;
    let excerpt = "abcdefghijklmnop correct long suffix with Unicode 猫 and original text.";
    let source = stored_message(
        &session.id,
        "repeated-user",
        Role::User,
        &format!("abcdefghijklmnop wrong suffix\n{excerpt}"),
    );
    store.append_batch(std::slice::from_ref(&source))?;
    let captured = view(
        &session.id,
        vec![stored_message(
            &session.id,
            "synthetic",
            Role::System,
            &format!("[{}]\n{excerpt}", source.id),
        )],
    );
    store.save_context_view(&captured)?;
    store.flush_serving()?;
    let encoded = stored_view(dir.path())?;
    assert!(!encoded.contains("correct long suffix"));
    assert_eq!(
        serde_json::to_vec(&store.context_view(&session.id)?.unwrap())?,
        serde_json::to_vec(&captured)?
    );
    Ok(())
}

#[test]
fn repetitive_selected_source_keeps_only_prefix_bounded_pointer_metadata() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("repetitive source")?;
    let source = stored_message(
        &session.id,
        "repetitive-user",
        Role::User,
        &"a".repeat(128 * 1024),
    );
    store.append_batch(std::slice::from_ref(&source))?;
    let mut clipped = source;
    clipped.content = format!("{}z", "a".repeat(1024));
    let captured = view(&session.id, vec![clipped]);
    store.save_context_view(&captured)?;
    store.flush_serving()?;
    assert_eq!(
        serde_json::to_vec(&store.context_view(&session.id)?.unwrap())?,
        serde_json::to_vec(&captured)?
    );
    let stored = stored_view(dir.path())?;
    assert!(stored.len() < 2000);
    assert!(!stored.contains(&"a".repeat(16)));
    Ok(())
}

#[test]
fn exact_large_canonical_prefix_uses_a_single_range_pointer() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(dir.path())?;
    let session = store.create_session("exact large prefix")?;
    let source = stored_message(
        &session.id,
        "exact-user",
        Role::User,
        &"a".repeat(128 * 1024),
    );
    store.append_batch(std::slice::from_ref(&source))?;
    let captured = view(&session.id, vec![source]);
    store.save_context_view(&captured)?;
    store.flush_serving()?;
    let encoded = stored_view(dir.path())?;
    assert!(encoded.len() < 1000);
    let encoded: serde_json::Value = serde_json::from_str(&encoded)?;
    assert_eq!(encoded["prefix_refs"][0]["kind"], "messages");
    assert_eq!(
        serde_json::to_vec(&store.context_view(&session.id)?.unwrap())?,
        serde_json::to_vec(&captured)?
    );
    Ok(())
}
