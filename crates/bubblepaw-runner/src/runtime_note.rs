//! Current runtime inventory for durable Generated notices and frozen views.
//! No state values, command output or child answers enter this inventory.
use crate::{agents::AgentTools, codemode::CodeMode, tools::ProcessTools};
use serde_json::Value;
use std::sync::{Arc, OnceLock, Weak};

const MAX_NOTE_BYTES: usize = 4096;

pub struct RuntimeNotes {
    repl: Arc<CodeMode>,
    processes: Option<Arc<ProcessTools>>,
    agents: OnceLock<Weak<AgentTools>>,
}

impl RuntimeNotes {
    pub fn new(repl: Arc<CodeMode>, processes: Arc<ProcessTools>) -> Self {
        Self {
            repl,
            processes: Some(processes),
            agents: OnceLock::new(),
        }
    }

    /// For hosts that register no process tools; the note then has no Commands line.
    pub fn without_processes(repl: Arc<CodeMode>) -> Self {
        Self {
            repl,
            processes: None,
            agents: OnceLock::new(),
        }
    }

    /// Bind after constructing child harnesses, without retaining an ownership cycle.
    pub fn set_agents(&self, agents: &Arc<AgentTools>) {
        let _ = self.agents.set(Arc::downgrade(agents));
    }

    /// Full rendering for a frozen snapshot: the LCM view freeze and explicit
    /// compaction. Saved key names and generations are stated here once.
    pub fn note(&self, session: &str) -> String {
        self.render(session, true)
    }

    /// Per-step rendering: only facts the model cannot get from its own results.
    /// Live cells, running or backgrounded commands and unread or running agents.
    /// With nothing live it is the same fixed text every time, so it stays quiet.
    pub fn live_note(&self, session: &str) -> String {
        self.render(session, false)
    }

    fn render(&self, session: &str, full: bool) -> String {
        // The runtime_inventory reminder kind owns restart dedup; this header is readable context.
        let mut note = format!(
            "{} (this process; quoted names are data):\n",
            bubblepaw_core::RUNTIME_INVENTORY_HEADER
        );
        let mut live = false;
        // Existing status uses mutex unwraps; a poisoned runtime must not turn a
        // context-preparation diagnostic into another failed run.
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.repl.status(session))) {
            Ok(snapshot) if full => append_environments(&mut note, &snapshot),
            Ok(snapshot) => live |= append_live_cells(&mut note, &snapshot),
            Err(_) => {
                note.push_str("REPL inventory unavailable.\n");
                live = true;
            }
        }
        if let Some(processes) = &self.processes {
            live |= append_jobs(
                &mut note,
                "Commands",
                &processes.continuation_status(session),
                false,
                full,
            );
        }
        match self.agents.get().and_then(Weak::upgrade) {
            Some(agents) => {
                live |= append_jobs(
                    &mut note,
                    "Agents",
                    &agents.continuation_status(session),
                    true,
                    full,
                )
            }
            None if full => note.push_str("Agents: no retained handles in this runtime.\n"),
            None => {}
        }
        if !full && !live {
            note.push_str("Runtime: no running commands, cells or agents.\n");
        }
        if note.len() > MAX_NOTE_BYTES {
            let suffix = "\n[Runtime note truncated; some facts omitted.]\n";
            let mut end = MAX_NOTE_BYTES - suffix.len();
            while !note.is_char_boundary(end) {
                end -= 1;
            }
            note.truncate(end);
            note.push_str(suffix);
        }
        note
    }
}

fn append_environments(note: &mut String, snapshot: &Value) {
    let Some(environments) = snapshot["environments"].as_array() else {
        note.push_str("REPL inventory unavailable.\n");
        return;
    };
    if environments.is_empty() {
        note.push_str("REPL: no retained environments or heap in this runtime.\n");
        return;
    }
    note.push_str(
        "REPL (saved key names from bounded snapshot; snapshot may omit or shorten names):\n",
    );
    let mut environments = environments.iter().collect::<Vec<_>>();
    environments.sort_by_key(|environment| environment["status"] == "idle");
    for environment in environments.iter().take(4) {
        let (name, _) = quoted(environment["name"].as_str().unwrap_or("?"), 70);
        let status = environment["status"].as_str().unwrap_or("unavailable");
        let generation = environment["generation"].as_u64().unwrap_or(0);
        let mut line = format!("  {name}: generation={generation}, status={status}, saved keys=[");
        let mut shown = 0;
        let mut shortened = false;
        if let Some(keys) = environment["state"].as_array() {
            for key in keys.iter().take(8) {
                let Some(key) = key["key"].as_str() else {
                    continue;
                };
                let (encoded, cut) = quoted(key, 96);
                if line.len() + encoded.len() > 330 {
                    break;
                }
                if shown > 0 {
                    line.push_str(", ");
                }
                line.push_str(&encoded);
                shortened |= cut || key.len() >= 128;
                shown += 1;
            }
            line.push(']');
            if keys.len() > shown {
                line.push_str(&format!(", {} snapshot keys omitted", keys.len() - shown));
            }
            if keys.len() >= 64 {
                line.push_str(", snapshot limit may omit more keys");
            }
        } else {
            line.push_str("] (inventory unavailable)");
        }
        if shortened {
            line.push_str(", key names may be shortened");
        }
        if environment["result_available"] == true {
            line.push_str(", cell result available");
        }
        line.push('\n');
        note.push_str(&line);
    }
    if environments.len() > 4 {
        note.push_str(&format!(
            "  {} environments omitted.\n",
            environments.len() - 4
        ));
    }
}

/// Per-step REPL facts: only cells that are running, waiting or backgrounded.
/// Returns whether any cell is live. Saved keys, generations and idle cells are
/// left to the full rendering.
fn append_live_cells(note: &mut String, snapshot: &Value) -> bool {
    let Some(environments) = snapshot["environments"].as_array() else {
        note.push_str("REPL inventory unavailable.\n");
        return true;
    };
    let live = environments
        .iter()
        .filter(|environment| {
            matches!(
                environment["status"].as_str(),
                Some("running" | "waiting" | "background")
            )
        })
        .collect::<Vec<_>>();
    if live.is_empty() {
        return false;
    }
    note.push_str("REPL cells (live):\n");
    for environment in live.iter().take(4) {
        let (name, _) = quoted(environment["name"].as_str().unwrap_or("?"), 70);
        let cell = match environment["status"].as_str() {
            Some("background") => "backgrounded",
            Some("waiting") => "waiting",
            _ => "running",
        };
        note.push_str(&format!("  {name}: {cell}\n"));
    }
    if live.len() > 4 {
        note.push_str(&format!("  {} REPL cells omitted.\n", live.len() - 4));
    }
    true
}

/// Lists handles the inventory supplies (already limited to live or unread
/// ones). The full rendering also states an empty list. Returns whether any
/// handle is listed.
fn append_jobs(
    note: &mut String,
    label: &str,
    inventory: &Value,
    agents: bool,
    full: bool,
) -> bool {
    if inventory["available"] != true {
        note.push_str(&format!("{label}: inventory unavailable.\n"));
        return true;
    }
    let entries = inventory["entries"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if entries.is_empty() {
        if full {
            note.push_str(&format!("{label}: no retained handles in this runtime.\n"));
        }
    } else {
        note.push_str(&format!("{label}:\n"));
        for entry in entries.iter().take(if agents { 4 } else { 8 }) {
            let (handle, cut) = quoted(entry["handle"].as_str().unwrap_or("?"), 64);
            let status = entry["status"].as_str().unwrap_or("unavailable");
            note.push_str(&format!("  {handle}: {status}"));
            if agents && entry["resultAvailable"] == true {
                note.push_str(", result available");
            }
            if entry["unread"] == true {
                note.push_str(", unread");
            }
            if let Some(exit) = entry["exitCode"].as_i64() {
                note.push_str(&format!(", exit={exit}"));
            }
            if cut {
                note.push_str(", handle shortened");
            }
            note.push('\n');
        }
    }
    let omitted = inventory["omitted"].as_u64().unwrap_or(0);
    if omitted > 0 {
        note.push_str(&format!("  {omitted} {label} handles omitted.\n"));
    }
    !entries.is_empty()
}

/// JSON quoting prevents a state key containing a newline from forging a fact.
fn quoted(value: &str, max_bytes: usize) -> (String, bool) {
    let mut text = String::new();
    let mut shortened = false;
    for character in value.chars() {
        text.push(character);
        if serde_json::to_string(&text).is_ok_and(|encoded| encoded.len() > max_bytes) {
            text.pop();
            shortened = true;
            break;
        }
    }
    (
        serde_json::to_string(&text).unwrap_or_else(|_| "\"?\"".into()),
        shortened,
    )
}

#[cfg(test)]
mod durable_inventory_tests {
    use super::*;
    use crate::{
        artifacts::ArtifactStore,
        provider::ChatCompletionsProvider,
        summarization::{SummarizingContext, SummaryConfig},
    };
    use bubblepaw_core::{
        Harness, HarnessConfig, MessageOrigin, Provider, Role, RunOptions, Store, ToolRegistry,
    };
    use serde_json::json;
    use std::sync::Mutex;

    #[tokio::test]
    async fn real_chat_wire_keeps_inventory_notices_in_position_and_unchanged_inventory_is_quiet()
    -> anyhow::Result<()> {
        let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
        let captured = seen.clone();
        let app = axum::Router::new().route("/chat/completions", axum::routing::post(move |axum::Json(request): axum::Json<Value>| {
            let seen = captured.clone();
            async move {
                let mut seen = seen.lock().unwrap();
                let index = seen.len();
                seen.push(request);
                let message = if index < 2 {
                    json!({"content":null,"reasoning_content":"native thought", "tool_calls":[{"id":format!("call-{index}"),"type":"function","function":{"name":"js","arguments":json!({"environment":"main","code":if index == 0 {"state.saved = 1; text('first');"} else {"text('second');"}}).to_string()}}]})
                } else {
                    json!({"content":"done","reasoning_content":"final thought"})
                };
                axum::Json(json!({"choices":[{"message":message,"finish_reason":if index < 2 {"tool_calls"} else {"stop"}}],"usage":{"prompt_tokens":100,"completion_tokens":10}}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let directory = tempfile::tempdir()?;
        let artifacts = Arc::new(ArtifactStore::new(
            directory.path(),
            directory.path().join("artifacts"),
        )?);
        let repl = Arc::new(CodeMode::new(directory.path().to_owned(), artifacts)?);
        let notes = Arc::new(RuntimeNotes::without_processes(repl.clone()));
        let provider: Arc<dyn Provider> = Arc::new(ChatCompletionsProvider::without_key(base)?);
        let store = Store::in_memory()?;
        let session = store.create_session("inventory wire")?.id;
        let context = Arc::new(SummarizingContext::new(
            store.clone(),
            provider.clone(),
            SummaryConfig::default(),
            Some(notes.clone()),
        )?);
        let context = Arc::new(crate::usage::UsageContext::new(store.clone(), context));
        let mut tools = ToolRegistry::new();
        repl.register(&mut tools)?;
        let harness = Harness::new(
            store.clone(),
            provider.clone(),
            tools,
            HarnessConfig {
                execution_mode: bubblepaw_core::ExecutionMode::Js,
                model: "test".into(),
                context,
                ..Default::default()
            },
        );
        let result = harness
            .prompt(&session, "Run two cells", RunOptions::default())
            .await?;
        assert_eq!(result.status, "completed", "{:?}", result.error);
        let before = store.messages(&session)?;
        let inventories = |messages: &[bubblepaw_core::Message]| {
            messages
                .iter()
                .filter(|message| {
                    message.content.contains(
                        "<harness kind=\"runtime_inventory\">Current volatile runtime facts",
                    )
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        let notices = inventories(&before);
        // Per-step notices carry live facts only. Two synchronous cells leave
        // nothing live, so one fixed notice is published and nothing repeats.
        assert_eq!(
            notices.len(),
            1,
            "one fixed nothing-live notice, no unchanged duplicate"
        );
        assert!(notices.iter().all(|message| message.role == Role::User
            && message.origin == Some(MessageOrigin::Generated)));
        assert!(
            notices[0]
                .content
                .contains("Runtime: no running commands, cells or agents.")
        );
        assert!(!notices[0].content.contains("\"saved\""));
        harness.release_idle_resources(&session).await?;
        assert_eq!(
            harness
                .prompt(&session, "Continue", RunOptions::default())
                .await?
                .status,
            "completed"
        );
        assert_eq!(
            inventories(&store.messages(&session)?).len(),
            1,
            "idle cache release must not republish unchanged inventory"
        );
        {
            let requests = seen.lock().unwrap();
            assert_eq!(requests.len(), 4);
            let previous = requests[1]["messages"].as_array().unwrap();
            let next = requests[2]["messages"].as_array().unwrap();
            assert_eq!(&next[..previous.len()], previous.as_slice());
            assert_eq!(
                next.iter()
                    .filter(|message| message["role"] == "system")
                    .count(),
                1
            );
            assert_eq!(
                next.iter()
                    .filter(|message| message["role"] == "tool")
                    .count(),
                2
            );
            assert_eq!(
                next.iter()
                    .filter_map(|message| message["tool_calls"].as_array())
                    .flatten()
                    .count(),
                2
            );
        }
        let child = store
            .create_session_with_parent("child inventory", Default::default(), Some(&session))?
            .id;
        assert_eq!(
            harness
                .prompt(&child, "Child task", RunOptions::default())
                .await?
                .status,
            "completed"
        );
        let child_notices = inventories(&store.messages(&child)?);
        assert_eq!(child_notices.len(), 1);
        assert!(
            child_notices[0]
                .content
                .contains("Runtime: no running commands, cells or agents.")
        );
        assert!(!child_notices[0].content.contains("\"saved\""));
        repl.shutdown().await;
        let fresh_repl = Arc::new(CodeMode::new(
            directory.path().to_owned(),
            Arc::new(ArtifactStore::new(
                directory.path(),
                directory.path().join("fresh"),
            )?),
        )?);
        let fresh_context = Arc::new(SummarizingContext::new(
            store.clone(),
            provider.clone(),
            SummaryConfig::default(),
            Some(Arc::new(RuntimeNotes::without_processes(
                fresh_repl.clone(),
            ))),
        )?);
        let fresh_context = Arc::new(crate::usage::UsageContext::new(
            store.clone(),
            fresh_context,
        ));
        let restarted = Harness::new(
            store.clone(),
            provider,
            ToolRegistry::new(),
            HarnessConfig {
                execution_mode: bubblepaw_core::ExecutionMode::Js,
                model: "test".into(),
                context: fresh_context,
                ..Default::default()
            },
        );
        assert_eq!(
            restarted
                .prompt(&session, "Resume", RunOptions::default())
                .await?
                .status,
            "completed"
        );
        let notices = inventories(&store.messages(&session)?);
        // The restart seeds from the stored notice, so unchanged live facts stay quiet.
        assert_eq!(
            notices.len(),
            1,
            "restart must not republish the fixed notice"
        );
        assert!(
            notices
                .last()
                .unwrap()
                .content
                .contains("Runtime: no running commands, cells or agents.")
        );
        assert!(!notices.last().unwrap().content.contains("\"saved\""));
        fresh_repl.shutdown().await;
        server.abort();
        Ok(())
    }
}
