use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ContextPreparation, Harness, HarnessConfig, Message, MessageOrigin,
    ModelRequest, ModelResponse, Provider, Reminder, Role, RunOptions, Store, TextCallback, Tool,
    ToolCall, ToolContext, ToolDefinition, ToolRegistry, ToolResult, is_typed_user,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

struct Notices {
    name: &'static str,
    pending: Mutex<HashMap<String, Vec<String>>>,
}
impl Notices {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            pending: Mutex::new(HashMap::new()),
        }
    }
    fn queue(&self, session: &str, text: &str) {
        self.pending
            .lock()
            .unwrap()
            .entry(session.into())
            .or_default()
            .push(text.into());
    }
}
#[async_trait]
impl Tool for Notices {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.into(),
            description: "scripted notice source".into(),
            input_schema: json!({"type":"object"}),
            read_only: true,
            approval: Default::default(),
        }
    }
    fn take_reminders(&self, session: &str) -> Vec<Reminder> {
        let texts = self
            .pending
            .lock()
            .unwrap()
            .remove(session)
            .unwrap_or_default();
        if texts.is_empty() {
            Vec::new()
        } else {
            vec![Reminder::new(
                format!("host_tool:{}", self.name),
                texts.join("\n"),
            )]
        }
    }
    async fn invoke(&self, context: ToolContext, _: Value) -> Result<ToolResult> {
        self.queue(
            &context.session_id,
            "process:active exited exitCode=0 stdout=8 stderr=0",
        );
        Ok(ToolResult::text("tool finished"))
    }
}

struct Recording {
    store: Store,
    call_once: bool,
    requests: Mutex<Vec<ModelRequest>>,
}
#[async_trait]
impl Provider for Recording {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        // The exact notice envelope must already be durable at dispatch time.
        let saved = self.store.messages(&request.session_id)?;
        for notice in request
            .messages
            .iter()
            .filter(|m| m.content.starts_with("<harness kind=\""))
        {
            assert_eq!(saved.iter().filter(|m| m.id == notice.id).count(), 1);
            assert_eq!(saved.iter().find(|m| m.id == notice.id), Some(notice));
            assert_eq!(notice.role, Role::User);
            assert_eq!(notice.origin, Some(MessageOrigin::Generated));
            assert!(!is_typed_user(notice));
        }
        let mut requests = self.requests.lock().unwrap();
        let first = requests.is_empty();
        requests.push(request);
        Ok(ModelResponse {
            text: "done".into(),
            tool_calls: if self.call_once && first {
                vec![ToolCall {
                    id: "notice-call".into(),
                    name: "model_notice".into(),
                    arguments: json!({}),
                }]
            } else {
                vec![]
            },
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn notices_batch_on_first_request_and_idle_next_turn_without_redelivery() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("notices")?.id;
    let model = Arc::new(Notices::new("model_notice"));
    let host = Arc::new(Notices::new("host_notice"));
    model.queue(
        &session,
        "process:first exited exitCode=0 stdout=1 stderr=0",
    );
    host.queue(
        &session,
        "process:second exited exitCode=2 stdout=0 stderr=4",
    );
    host.queue("other-session", "must stay in its own session");
    let mut registry = ToolRegistry::new();
    registry.register(model.clone())?;
    registry.register_host(host.clone())?;
    let provider = Arc::new(Recording {
        store: store.clone(),
        call_once: false,
        requests: Mutex::new(vec![]),
    });
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        registry,
        HarnessConfig::default(),
    );
    let before = store.storage_stats()?.chat_transaction_count;
    assert_eq!(
        harness
            .prompt(&session, "first", RunOptions::default())
            .await?
            .status,
        "completed"
    );
    assert_eq!(store.storage_stats()?.chat_transaction_count - before, 3);
    let messages = store.messages(&session)?;
    assert_eq!(messages.len(), 3);
    assert_eq!(
        messages[1].content,
        "<harness kind=\"host_tool:model_notice\">process:first exited exitCode=0 stdout=1 stderr=0</harness>\n<harness kind=\"host_tool:host_notice\">process:second exited exitCode=2 stdout=0 stderr=4</harness>"
    );
    assert_eq!(
        provider.requests.lock().unwrap()[0].messages.last(),
        Some(&messages[1])
    );
    assert!(model.take_reminders(&session).is_empty());
    assert!(host.take_reminders(&session).is_empty());
    assert_eq!(
        host.take_reminders("other-session")
            .into_iter()
            .map(|reminder| reminder.text)
            .collect::<Vec<_>>(),
        vec!["must stay in its own session"]
    );

    let before = store.storage_stats()?.chat_transaction_count;
    harness
        .prompt(&session, "no new notice", RunOptions::default())
        .await?;
    assert_eq!(store.storage_stats()?.chat_transaction_count - before, 2);
    assert_eq!(
        store
            .messages(&session)?
            .iter()
            .filter(|m| m.content.starts_with("<harness kind=\""))
            .count(),
        1
    );

    host.queue(&session, "process:idle exited exitCode=0 stdout=9 stderr=0");
    assert_eq!(
        provider.requests.lock().unwrap().len(),
        2,
        "idle notices must not wake the model"
    );
    harness
        .prompt(&session, "next turn", RunOptions::default())
        .await?;
    let messages = store.messages(&session)?;
    let notices: Vec<_> = messages
        .iter()
        .filter(|m| m.content.starts_with("<harness kind=\""))
        .collect();
    assert_eq!(notices.len(), 2);
    assert_eq!(
        notices[1].content,
        "<harness kind=\"host_tool:host_notice\">process:idle exited exitCode=0 stdout=9 stderr=0</harness>"
    );
    assert_eq!(
        provider.requests.lock().unwrap()[2].messages.last(),
        Some(notices[1])
    );
    Ok(())
}

#[tokio::test]
async fn notice_after_tool_round_preserves_call_result_adjacency() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("active")?.id;
    let tool = Arc::new(Notices::new("model_notice"));
    let mut registry = ToolRegistry::new();
    registry.register(tool.clone())?;
    let provider = Arc::new(Recording {
        store: store.clone(),
        call_once: true,
        requests: Mutex::new(vec![]),
    });
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        registry,
        HarnessConfig::default(),
    );
    assert_eq!(
        harness
            .prompt(&session, "use tool", RunOptions::default())
            .await?
            .status,
        "completed"
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let history = &requests[1].messages;
    assert_eq!(history.len(), 4);
    assert_eq!(history[1].tool_calls[0].id, "notice-call");
    assert_eq!(history[2].role, Role::Tool);
    assert_eq!(history[2].tool_call_id.as_deref(), Some("notice-call"));
    assert_eq!(
        history[3].content,
        "<harness kind=\"host_tool:model_notice\">process:active exited exitCode=0 stdout=8 stderr=0</harness>"
    );
    assert!(tool.take_reminders(&session).is_empty());
    assert_eq!(
        store
            .messages(&session)?
            .iter()
            .filter(|m| m.content == history[3].content)
            .count(),
        1
    );
    Ok(())
}

struct StripHistory;
impl ContextPreparation for StripHistory {
    fn project_history(&self, _: &[Message]) -> Result<Vec<Message>> {
        Ok(vec![])
    }
}

#[tokio::test]
async fn admitted_generated_host_input_survives_context_replacement() -> Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("admitted host input")?.id;
    let provider = Arc::new(Recording {
        store: store.clone(),
        call_once: false,
        requests: Mutex::new(vec![]),
    });
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        ToolRegistry::new(),
        HarnessConfig {
            context: Arc::new(StripHistory),
            ..Default::default()
        },
    );
    let notice = "[host notice]\nChild fixture completed.";
    let run = harness
        .prompt(
            &session,
            notice,
            RunOptions {
                origin: MessageOrigin::Generated,
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(run.status, "completed");
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests[0].messages.len(), 1);
        assert!(
            requests[0].messages[0]
                .content
                .ends_with(&format!(">{notice}</harness>"))
        );
        assert_eq!(
            requests[0].messages[0].origin,
            Some(MessageOrigin::Generated)
        );
    }
    assert_eq!(
        store
            .messages(&session)?
            .iter()
            .filter(|message| message.content.ends_with(&format!(">{notice}</harness>")))
            .count(),
        1
    );
    // A typed lookalike remains ordinary user input under the host's policy.
    harness
        .prompt(&session, notice, RunOptions::default())
        .await?;
    assert!(provider.requests.lock().unwrap()[1].messages.is_empty());
    Ok(())
}

#[tokio::test]
async fn context_policy_cannot_strip_notice_and_final_size_guard_includes_it() -> Result<()> {
    let max_request_bytes = 4096;
    let store = Store::in_memory()?;
    let session = store.create_session("context")?.id;
    let tool = Arc::new(Notices::new("host_notice"));
    let mut registry = ToolRegistry::new();
    registry.register_host(tool.clone())?;
    let provider = Arc::new(Recording {
        store: store.clone(),
        call_once: false,
        requests: Mutex::new(vec![]),
    });
    let harness = Harness::new(
        store.clone(),
        provider.clone(),
        registry,
        HarnessConfig {
            context: Arc::new(StripHistory),
            max_request_bytes,
            ..Default::default()
        },
    );
    tool.queue(&session, "process:kept exited exitCode=0 stdout=0 stderr=0");
    let first = harness
        .prompt(&session, "first", RunOptions::default())
        .await?;
    assert_eq!(first.status, "completed", "{:?}", first.error);
    {
        let requests = provider.requests.lock().unwrap();
        assert!(serde_json::to_vec(&requests[0])?.len() < max_request_bytes);
        assert_eq!(requests[0].messages.len(), 1);
        assert_eq!(
            requests[0].messages[0].content,
            "<harness kind=\"host_tool:host_notice\">process:kept exited exitCode=0 stdout=0 stderr=0</harness>"
        );
    }
    let oversized = "oversized".repeat(1024);
    assert!(oversized.len() > max_request_bytes);
    tool.queue(&session, &oversized);
    let failed = harness
        .prompt(&session, "second", RunOptions::default())
        .await?;
    assert_eq!(failed.status, "failed");
    assert!(failed.error.unwrap().contains(&format!(
        "Prepared request exceeds configured {max_request_bytes} byte limit"
    )));
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert_eq!(
        store
            .messages(&session)?
            .iter()
            .filter(|m| m.content.starts_with("<harness kind=\""))
            .count(),
        1
    );
    Ok(())
}
