use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ContextView, EventKind, Harness, HarnessConfig, HarnessEvent, InputKind,
    Message, MessageOrigin, ModelRequest, ModelResponse, Provider, RunOptions, Session, Store,
    TextCallback, ToolRegistry,
};
use rusqlite::{Connection, params};
use serde_json::json;
use std::{path::Path, sync::Arc};

// These fixtures contain the exact initialization SQL from the two pre-merge
// stores, rather than creating an old database with the merged implementation.
const MAIN_SCHEMA: &str = include_str!("fixtures/main-v6-schema.sql");
const AGENT_SCHEMA: &str = include_str!("fixtures/agent-v4-schema.sql");
const DATE: &str = "2026-10-08T00:00:00Z";

struct Model;
#[async_trait]
impl Provider for Model {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        Ok(ModelResponse {
            text: "new answer".into(),
            ..Default::default()
        })
    }
}

fn fixture(path: &Path, schema: &str) -> Result<(Session, Message)> {
    let conn = Connection::open(path)?;
    conn.execute_batch(schema)?;
    let session: Session = serde_json::from_value(json!({
        "id": "old-session", "title": "Old session", "parent_session_id": null,
        "parent_cutoff": null, "created_at": DATE, "updated_at": DATE, "read_only": false,
    }))?;
    let message: Message = serde_json::from_value(json!({
        "id": "old-message", "session_id": session.id, "role": "user",
        "content": "original history", "created_at": DATE,
    }))?;
    conn.execute(
        "INSERT INTO sessions VALUES(?1,?2)",
        params![session.id, serde_json::to_string(&session)?],
    )?;
    conn.execute(
        "INSERT INTO messages(session_id,data) VALUES(?1,?2)",
        params![session.id, serde_json::to_string(&message)?],
    )?;
    conn.execute(
        "INSERT INTO inputs(id,session_id,kind,text,state) VALUES('old-input',?1,'followup','old pending input','pending')",
        [&session.id],
    )?;
    Ok((session, message))
}

fn assert_schema(store: &Store, session_id: &str) -> Result<()> {
    let conn = Connection::open(store.chat_path(session_id)?.unwrap())?;
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?,
        11
    );
    for table in [
        "context_episodes",
        "context_subject_chapters",
        "context_decisions",
        "approvals",
        "approval_rules",
    ] {
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |r| r.get::<_, i64>(0),
            )?,
            1,
            "missing {table}"
        );
    }
    let mut stmt = conn.prepare("PRAGMA foreign_key_check")?;
    assert!(stmt.query([])?.next()?.is_none());
    let serving = Connection::open(store.data_dir().unwrap().join("serving.db"))?;
    for table in ["context_views", "events"] {
        assert_eq!(
            serving.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |r| r.get::<_, i64>(0)
            )?,
            1
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |r| r.get::<_, i64>(0)
            )?,
            0
        );
    }
    Ok(())
}

#[test]
fn main_v6_preserves_frozen_context_and_adds_agent_tables() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("main.sqlite");
    let (session, message) = fixture(&path, MAIN_SCHEMA)?;
    let frozen = ContextView {
        session_id: session.id.clone(),
        frontier: Some(1),
        system_prompt: "frozen instructions".into(),
        prefix: vec![message.clone()],
        masked_tool_output_ids: vec![],
        clipped_tool_call_ids: vec![],
        created_at: DATE.into(),
        threshold: 200,
        budget: 100,
        tokens: 20,
    };
    let conn = Connection::open(&path)?;
    // Populate the main-only dependency chain so deletion must account for it.
    let chapter = json!({
        "id": "chapter", "session_id": session.id, "ordinal": 1,
        "first_episode_ordinal": 1, "last_episode_ordinal": 1, "status": "open",
        "related_ids": [], "related_evidence": [],
    });
    let episode = json!({
        "id": "episode", "session_id": session.id, "ordinal": 1,
        "first_message_id": message.id, "last_message_id": message.id,
        "source_tokens": 32, "cap": 20, "origin_request_id": null,
        "title": "Old episode", "summary": "frozen episode", "model": "old-model",
        "usage": null, "created_at": DATE, "chapter_id": "chapter",
    });
    let decision = json!({
        "id": "decision", "session_id": session.id, "kind": "split", "episode_ordinal": 1,
        "option_message_ids": [message.id], "probabilities": [0.0, 1.0], "choice": null,
        "model": "old-model", "usage": null, "created_at": DATE,
    });
    conn.execute(
        "INSERT INTO context_subject_chapters VALUES('chapter',?1,1,1,1,'open',?2)",
        params![session.id, chapter.to_string()],
    )?;
    conn.execute(
        "INSERT INTO context_episodes VALUES('episode',?1,1,1,1,'chapter',?2)",
        params![session.id, episode.to_string()],
    )?;
    conn.execute(
        "INSERT INTO context_decisions VALUES('decision',?1,1,'split',?2)",
        params![session.id, decision.to_string()],
    )?;
    conn.execute(
        "INSERT INTO context_views VALUES(?1,?2)",
        params![session.id, serde_json::to_string(&frozen)?],
    )?;
    drop(conn);

    let store = Store::open(&path)?;
    assert_eq!(store.storage_stats()?.chat_transaction_count, 0);
    assert_eq!(store.messages(&session.id)?[0].content, message.content);
    assert_eq!(
        serde_json::to_value(store.context_view(&session.id)?.unwrap())?,
        serde_json::to_value(&frozen)?
    );
    assert_eq!(
        serde_json::to_value(&store.context_episodes(&session.id)?[0])?["summary"],
        episode["summary"]
    );
    assert_eq!(store.context_subject_chapters(&session.id)?.len(), 1);
    assert_eq!(store.context_decisions(&session.id)?.len(), 1);
    store.queue_with_origin(
        &session.id,
        InputKind::Steer,
        "new input",
        MessageOrigin::Generated,
    )?;
    assert_schema(&store, &session.id)?;
    let chat = store.chat_path(&session.id)?.unwrap();
    store.rename_session(&session.id, "Renamed")?;
    store.delete_session(&session.id)?;
    assert!(store.list_sessions()?.is_empty());
    assert!(!chat.exists());
    drop(store);
    assert!(Store::open(&path)?.list_sessions()?.is_empty());
    Ok(())
}

#[tokio::test]
async fn agent_v4_preserves_approvals_and_event_cursor_while_adding_episode_schema() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("agent.sqlite");
    let (session, message) = fixture(&path, AGENT_SCHEMA)?;
    let event = HarnessEvent {
        sequence: 41,
        durable: true,
        origin: None,
        session_id: session.id.clone(),
        run_id: "old-run".into(),
        created_at: DATE.into(),
        kind: EventKind::SessionSettled {},
    };
    let conn = Connection::open(&path)?;
    conn.execute(
        "INSERT INTO approval_rules VALUES(?1,'old-tool')",
        [&session.id],
    )?;
    conn.execute("INSERT INTO approvals VALUES('old-approval',?1,'old-run','old-call','old-tool','{}','approved',NULL,?2,?2)", params![session.id, DATE])?;
    conn.execute(
        "INSERT INTO events VALUES(41,?1,'old-run',?2,'session_settled',?3)",
        params![session.id, DATE, serde_json::to_string(&event)?],
    )?;
    drop(conn);

    let store = Store::open(&path)?;
    assert_eq!(store.storage_stats()?.chat_transaction_count, 0);
    assert_eq!(store.messages(&session.id)?[0].content, message.content);
    assert_eq!(store.queued_inputs(&session.id)?[0].origin, None);
    assert_schema(&store, &session.id)?;
    let agent = Harness::new(
        store.clone(),
        Arc::new(Model),
        ToolRegistry::new(),
        HarnessConfig::default(),
    );
    assert!(agent.last_sequence() > 41);
    assert!(!agent.can_resume_from(41));
    assert_eq!(
        serde_json::to_value(&agent.events_after(0, Some(&session.id), 10)?[0])?,
        serde_json::to_value(&event)?
    );
    agent
        .prompt(&session.id, "continue", RunOptions::default())
        .await?;
    assert!(agent.last_sequence() > 41);
    let conn = Connection::open(store.data_dir().unwrap().join("serving.db"))?;
    assert_eq!(
        conn.query_row(
            "SELECT state FROM approvals WHERE id='old-approval'",
            [],
            |r| r.get::<_, String>(0)
        )?,
        "approved"
    );
    let chat = Connection::open(store.chat_path(&session.id)?.unwrap())?;
    assert_eq!(
        chat.query_row("SELECT COUNT(*) FROM approval_rules", [], |r| r
            .get::<_, i64>(0))?,
        1
    );
    let last = agent.last_sequence();
    drop(conn);
    drop(agent);
    drop(store);
    let reopened = Store::open(&path)?;
    let agent = Harness::new(
        reopened,
        Arc::new(Model),
        ToolRegistry::new(),
        HarnessConfig::default(),
    );
    assert!(agent.last_sequence() > last);
    assert!(!agent.can_resume_from(last));
    Ok(())
}
