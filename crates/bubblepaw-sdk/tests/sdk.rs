//! The SDK's behavior: a scripted chat, the support-tool merge, the Claude
//! Code helper and the public surface.

use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_sdk::approvals::Approval;
use bubblepaw_sdk::context::SummaryConfig;
use bubblepaw_sdk::harness::{HarnessConfig, RunOptions};
use bubblepaw_sdk::providers::{ProviderRegistry, ScriptedProvider, ScriptedStep};
use bubblepaw_sdk::store::{Role, Store};
use bubblepaw_sdk::tools::{
    ArtifactStore, CodeMode, Tool, ToolCall, ToolContext, ToolDefinition, ToolRegistry, ToolResult,
    register_history_tools,
};
use bubblepaw_sdk::{HostBuilder, claude_code};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

/// A host tool that answers with its own input and records that it ran.
struct Echo {
    ran: Arc<AtomicBool>,
}

#[async_trait]
impl Tool for Echo {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "echo".into(),
            description: "Return the given text unchanged.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"],
                "additionalProperties": false
            }),
            read_only: true,
            approval: Approval::None,
        }
    }

    async fn invoke(&self, _context: ToolContext, input: Value) -> Result<ToolResult> {
        self.ran.store(true, Ordering::SeqCst);
        let text = input
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default();
        Ok(ToolResult::text(text))
    }
}

/// Marks every tool it wraps, so the test can see where the wrapper ran.
struct Wrapped {
    inner: Arc<dyn Tool>,
}

#[async_trait]
impl Tool for Wrapped {
    fn definition(&self) -> ToolDefinition {
        let mut definition = self.inner.definition();
        definition.description = format!("wrapped: {}", definition.description);
        definition
    }

    async fn invoke(&self, context: ToolContext, input: Value) -> Result<ToolResult> {
        self.inner.invoke(context, input).await
    }

    async fn invoke_with_dispatcher(
        &self,
        context: ToolContext,
        input: Value,
        dispatcher: bubblepaw_sdk::tools::ToolDispatcher,
    ) -> Result<ToolResult> {
        self.inner
            .invoke_with_dispatcher(context, input, dispatcher)
            .await
    }
}

fn scripted(steps: Vec<ScriptedStep>, final_text: &str) -> ProviderRegistry {
    ProviderRegistry::new(
        "scripted",
        Arc::new(ScriptedProvider::with_steps(steps, final_text)),
    )
}

fn scripted_config() -> HarnessConfig {
    HarnessConfig {
        model: "scripted".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn sdk_scripted_chat_runs_host_tool() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let session = store.create_session("sdk scripted chat")?;

    let ran = Arc::new(AtomicBool::new(false));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Echo { ran: ran.clone() }))?;

    let steps = vec![ScriptedStep::Calls {
        calls: vec![ToolCall {
            id: "call-echo-1".into(),
            name: "echo".into(),
            arguments: json!({"text": "hello from the host tool"}),
        }],
    }];

    let host = HostBuilder::new(store.clone(), scripted(steps, "The echo tool answered."))
        .workspace(workspace.path())
        .tools(tools)
        .history_tools(true)
        .summarize(SummaryConfig::default())
        .config(scripted_config())
        .build()?;

    host.harness
        .start(&session.id, "Use the echo tool.", RunOptions::default())?;
    let run = host.harness.wait_for_idle(&session.id).await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    assert!(ran.load(Ordering::SeqCst), "the echo tool did not run");
    assert_eq!(run.text.as_deref(), Some("The echo tool answered."));

    let messages = store.messages(&session.id)?;
    let answer = messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant)
        .expect("a final assistant message");
    assert_eq!(answer.content, "The echo tool answered.");
    let tool_result = messages
        .iter()
        .find(|message| message.role == Role::Tool)
        .expect("a tool result");
    assert!(!tool_result.is_error, "{}", tool_result.content);
    assert!(tool_result.content.contains("hello from the host tool"));
    Ok(())
}

#[tokio::test]
async fn sdk_todo_tool_updates_host_state() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let session = store.create_session("sdk todo")?;
    let items = json!([
        {"text": "port the gateway", "status": "in_progress"},
        {"text": "bump the pin", "status": "pending"}
    ]);
    let steps = vec![ScriptedStep::Calls {
        calls: vec![ToolCall {
            id: "call-todo-1".into(),
            name: "todo".into(),
            arguments: json!({"items": items.clone()}),
        }],
    }];
    let host = HostBuilder::new(store.clone(), scripted(steps, "Planned."))
        .workspace(workspace.path())
        .todo(true)
        .config(scripted_config())
        .build()?;
    let todo = host
        .tools
        .with_execution_mode(bubblepaw_sdk::harness::ExecutionMode::Native)
        .definitions()
        .into_iter()
        .find(|definition| definition.name == "todo")
        .expect("todo is a native model tool");
    assert_eq!(todo.input_schema["type"], "object");

    host.harness
        .start(&session.id, "Plan the work.", RunOptions::default())?;
    let run = host.harness.wait_for_idle(&session.id).await?;
    assert_eq!(run.status, "completed", "{:?}", run.error);
    let saved = host.todo(&session.id)?;
    assert_eq!(saved.len(), 2);
    assert_eq!(saved[0].status, bubblepaw_sdk::todo::TodoStatus::InProgress);
    assert_eq!(serde_json::to_value(&saved)?, items);
    let changed = host
        .harness
        .events_after(0, Some(&session.id), 1000)?
        .into_iter()
        .any(|event| matches!(&event.kind, bubblepaw_sdk::events::EventKind::HostStateChanged { key, value } if key == bubblepaw_sdk::todo::HOST_STATE_KEY && value == &items));
    assert!(changed, "no todo host_state_changed event");
    Ok(())
}

#[tokio::test]
async fn sdk_builder_wraps_support_tools() -> Result<()> {
    let workspace = tempfile::tempdir()?;
    let store = Store::in_memory()?;
    let session = store.create_session("sdk support tools")?;
    let artifacts_dir = workspace.path().join("artifacts");

    // The reference merge: the same support tools registered without a builder.
    let mut expected_registry = ToolRegistry::new();
    register_history_tools(&mut expected_registry, store.clone())?;
    let reference = tempfile::tempdir()?;
    let artifacts = Arc::new(ArtifactStore::new(
        reference.path(),
        reference.path().join("artifacts"),
    )?);
    let scratch = Arc::new(CodeMode::new(reference.path().to_path_buf(), artifacts)?);
    scratch.register(&mut expected_registry)?;
    let mut expected: Vec<String> = expected_registry
        .capabilities()
        .into_iter()
        .map(|definition| definition.name)
        .collect();
    expected.sort();
    assert!(!expected.is_empty(), "the support tools must exist");

    let wrapped = Arc::new(Mutex::new(Vec::new()));
    let recorder = wrapped.clone();
    let host = HostBuilder::new(
        store.clone(),
        scripted(Vec::new(), "The support tools answered."),
    )
    .workspace(workspace.path())
    .history_tools(true)
    .code_mode(&artifacts_dir)
    .wrap_support_tools(move |tool| {
        recorder.lock().unwrap().push(tool.definition().name);
        Arc::new(Wrapped { inner: tool })
    })
    .config(scripted_config())
    .build()?;

    let mut wrapped_names = wrapped.lock().unwrap().clone();
    wrapped_names.sort();
    assert_eq!(
        wrapped_names, expected,
        "every history and code-mode host tool is wrapped once"
    );
    let mut published: Vec<String> = host
        .tools
        .capabilities()
        .into_iter()
        .map(|definition| definition.name)
        .collect();
    published.sort();
    assert_eq!(
        published, expected,
        "Host::tools is the final registry the harness was built with"
    );

    // The wrapped tools still reach the model with their host status intact.
    let definitions = host
        .harness
        .effective_tool_definitions(&session.id, &RunOptions::default())?;
    for name in &expected {
        let definition = definitions
            .iter()
            .find(|definition| &definition.name == name)
            .unwrap_or_else(|| panic!("{name} is missing from the native declarations"));
        assert!(
            definition.description.contains("wrapped: "),
            "{name} did not go through the wrapper"
        );
        assert!(
            !expected_registry.is_shown_always(name),
            "{name} is not a shown-always support tool"
        );
    }
    Ok(())
}

#[test]
fn sdk_claude_code_bundled_dir_exists() {
    let bundled = claude_code::bundled_dir();
    let main = bundled.join("src/main.ts");
    assert!(
        main.is_file(),
        "expected the bundled Claude Code extension at {}",
        main.display()
    );
    let config = claude_code::extension_config(
        "claude-code",
        std::path::Path::new("bun"),
        bundled.as_path(),
    );
    assert_eq!(config.id, "claude-code");
    assert_eq!(config.command, "bun");
    assert_eq!(config.args.first().map(String::as_str), Some("run"));
    assert_eq!(config.cwd.as_deref(), Some(bundled.as_path()));
    assert!(config.account.is_none());
    assert!(config.env.is_empty());
}

/// Names every re-export, so a removed or renamed item fails to compile.
#[test]
fn sdk_surface_names_every_public_item() {
    // store
    let _: Option<bubblepaw_sdk::store::Store> = None;
    let _: Option<bubblepaw_sdk::store::Session> = None;
    let _: Option<bubblepaw_sdk::store::Message> = None;
    let _: Option<bubblepaw_sdk::store::Role> = None;
    let _: Option<bubblepaw_sdk::store::MessageOrigin> = None;
    let _: Option<bubblepaw_sdk::store::ContextChapter> = None;
    let _: Option<bubblepaw_sdk::store::LogPosition> = None;
    let _: Option<bubblepaw_sdk::store::QueuedInput> = None;
    let _ = bubblepaw_sdk::store::EVENTS_AFTER_LIMIT;

    // harness
    let _: Option<bubblepaw_sdk::harness::Harness> = None;
    let _: Option<bubblepaw_sdk::harness::HarnessConfig> = None;
    let _: Option<bubblepaw_sdk::harness::RunOptions> = None;
    let _: Option<bubblepaw_sdk::harness::ExecutionMode> = None;
    let _: Option<bubblepaw_sdk::harness::SessionBusy> = None;
    let _: Option<bubblepaw_sdk::harness::ClientInfo> = None;
    let _: Option<bubblepaw_sdk::harness::CancellationToken> = None;

    // events
    let _: Option<bubblepaw_sdk::events::HarnessEvent> = None;
    let _: Option<bubblepaw_sdk::events::EventKind> = None;

    // approvals
    let _: Option<bubblepaw_sdk::approvals::ApprovalPolicy> = None;
    let _: Option<bubblepaw_sdk::approvals::ApprovalMode> = None;
    let _: Option<bubblepaw_sdk::approvals::Approval> = None;
    let _: Option<bubblepaw_sdk::approvals::Decision> = None;
    let _: Option<bubblepaw_sdk::approvals::ApprovalRecord> = None;

    // tools
    let _: Option<&dyn bubblepaw_sdk::tools::Tool> = None;
    let _: Option<bubblepaw_sdk::tools::ToolRegistry> = None;
    let _: Option<bubblepaw_sdk::tools::ToolDefinition> = None;
    let _: Option<bubblepaw_sdk::tools::ToolResult> = None;
    let _: Option<bubblepaw_sdk::tools::ToolCall> = None;
    let _: Option<bubblepaw_sdk::tools::ToolContext> = None;
    let _: Option<bubblepaw_sdk::tools::ToolDispatcher> = None;
    let _: Option<bubblepaw_sdk::tools::JsonSchema> = None;
    let _: Option<bubblepaw_sdk::tools::WorkspaceTools> = None;
    let _: Option<bubblepaw_sdk::tools::ProcessTools> = None;
    let _: Option<bubblepaw_sdk::tools::CodeMode> = None;
    let _: Option<bubblepaw_sdk::tools::ArtifactStore> = None;
    let _: Option<bubblepaw_sdk::tools::RuntimeNotes> = None;
    let _ = bubblepaw_sdk::tools::register_history_tools;

    // todo
    let _: Option<bubblepaw_sdk::todo::TodoItem> = None;
    let _: Option<bubblepaw_sdk::todo::TodoStatus> = None;
    let _ = bubblepaw_sdk::todo::list;
    let _ = bubblepaw_sdk::todo::register;
    let _ = bubblepaw_sdk::todo::MAX_TODO_ITEMS;
    let _ = bubblepaw_sdk::todo::HOST_STATE_KEY;
    // The native registration helper is generic over its two path arguments, so
    // name it with concrete ones.
    fn register_native_tools_with_exec(
        registry: &mut bubblepaw_sdk::tools::ToolRegistry,
        root: std::path::PathBuf,
        read_only: bool,
        exec: bubblepaw_sdk::runner::config::ExecConfig,
        data_dir: std::path::PathBuf,
    ) -> anyhow::Result<std::sync::Arc<bubblepaw_sdk::tools::ProcessTools>> {
        bubblepaw_sdk::tools::register_native_tools_with_exec(
            registry, root, read_only, exec, data_dir,
        )
    }
    let _ = register_native_tools_with_exec;

    // providers
    let _: Option<&dyn bubblepaw_sdk::providers::Provider> = None;
    let _: Option<bubblepaw_sdk::providers::ModelRequest> = None;
    let _: Option<bubblepaw_sdk::providers::ModelResponse> = None;
    let _: Option<bubblepaw_sdk::providers::TextCallback> = None;
    let _: Option<bubblepaw_sdk::providers::Usage> = None;
    let _: Option<bubblepaw_sdk::providers::ProviderError> = None;
    let _: Option<bubblepaw_sdk::providers::ProviderErrorKind> = None;
    let _: Option<bubblepaw_sdk::providers::ReasoningEffort> = None;
    let _: Option<bubblepaw_sdk::providers::DecisionRequest> = None;
    let _: Option<bubblepaw_sdk::providers::DecisionResponse> = None;
    let _: Option<bubblepaw_sdk::providers::DecisionChoice> = None;
    let _: Option<&dyn bubblepaw_sdk::providers::DecisionProvider> = None;
    let _: Option<bubblepaw_sdk::providers::DecisionUsage> = None;
    let _: Option<bubblepaw_sdk::providers::Question> = None;
    let _: Option<bubblepaw_sdk::providers::QuestionKind> = None;
    let _: Option<bubblepaw_sdk::providers::Answer> = None;
    let _: Option<bubblepaw_sdk::providers::AnswerKind> = None;
    let _: Option<bubblepaw_sdk::providers::ProviderRegistry> = None;
    let _: Option<bubblepaw_sdk::providers::ProviderEndpoints> = None;
    let _: Option<bubblepaw_sdk::providers::ResponsesProvider> = None;
    let _: Option<bubblepaw_sdk::providers::ExtensionProvider> = None;
    let _: Option<bubblepaw_sdk::providers::ExtensionConfig> = None;
    let _: Option<bubblepaw_sdk::providers::ScriptedProvider> = None;
    let _: Option<bubblepaw_sdk::providers::ScriptedStep> = None;
    let _: Option<bubblepaw_sdk::providers::ModelRole> = None;

    // context
    let _: Option<&dyn bubblepaw_sdk::context::ContextPreparation> = None;
    let _: Option<bubblepaw_sdk::context::PrepareCall<'static>> = None;
    let _: Option<bubblepaw_sdk::context::PrepareMode> = None;
    let _: Option<bubblepaw_sdk::context::PreparedContext> = None;
    let _: Option<bubblepaw_sdk::context::ContextWriter<'static>> = None;
    let _: Option<bubblepaw_sdk::context::SummarizingContext> = None;
    let _: Option<bubblepaw_sdk::context::SummaryConfig> = None;
    let _ = bubblepaw_sdk::context::cached_context_window;

    // net and the web host
    let _: Option<bubblepaw_sdk::net::WebConfig> = None;
    let _: Option<bubblepaw_sdk::net::SearchBackend> = None;
    #[cfg(feature = "web")]
    let _: Option<bubblepaw_sdk::web::WebConfig> = None;

    // Claude Code helpers, the host, the escape hatches and the prelude
    let _ = bubblepaw_sdk::claude_code::bundled_dir;
    let _ = bubblepaw_sdk::claude_code::extension_config;
    let _: Option<bubblepaw_sdk::Host> = None;
    let _: Option<bubblepaw_sdk::HostBuilder> = None;
    let _: Option<bubblepaw_sdk::core::Store> = None;
    let _: Option<bubblepaw_sdk::runner::provider::ProviderRegistry> = None;
    let _: Option<bubblepaw_sdk::prelude::Store> = None;
    let _: Option<bubblepaw_sdk::prelude::HostBuilder> = None;
}
