//! The ordinary embedder wiring, and the host it builds.

use crate::host_api::{self, HostCtx, Profile, ReadScope, dispatch};
use crate::questions::{ChildQuestionAnswerError, PendingChildQuestion, QuestionTools};
use anyhow::{Context, Result, ensure};
use bubblepaw_core::{
    ContextPreparation, Harness, HarnessConfig, Provider, Store, Tool, ToolRegistry,
};
use bubblepaw_runner::{
    artifacts::ArtifactStore,
    codemode::CodeMode,
    provider::ProviderRegistry,
    runtime_note::RuntimeNotes,
    summarization::{SummarizingContext, SummaryConfig},
    todo::{self, TodoItem},
    tools::register_history_tools,
};
use serde_json::json;
use std::{path::PathBuf, sync::Arc};

type Wrapper = Arc<dyn Fn(Arc<dyn Tool>) -> Arc<dyn Tool> + Send + Sync>;
type ContextWrapper = Arc<dyn Fn(SummarizingContext) -> Arc<dyn ContextPreparation> + Send + Sync>;

/// A built host: the harness plus the pieces an embedder keeps using.
pub struct Host {
    /// The underlying harness. Host operations are available through [`Self::api`].
    pub harness: Arc<Harness>,
    /// The authoritative store the harness persists to.
    pub store: Store,
    /// The provider registry the harness routes through.
    pub providers: Arc<ProviderRegistry>,
    /// The JavaScript adapter, when [`HostBuilder::code_mode`] was used.
    pub code_mode: Option<Arc<CodeMode>>,
    /// A snapshot of the final registry the harness was built with, for hosts
    /// that also serve its tools outside a run (a gateway, tool listings).
    pub tools: ToolRegistry,
    api: HostCtx,
}

impl Host {
    /// Shared host context for typed or JSON method dispatch.
    pub fn api(&self) -> &HostCtx {
        &self.api
    }

    /// The chat's latest todo list; empty when the model never set one.
    pub fn todo(&self, session_id: &str) -> Result<Vec<TodoItem>> {
        host_api::sessions::todo(&self.api, session_id).map_err(Into::into)
    }

    /// Read a child's durable pending question without opening archived chats.
    pub fn pending_child_question(&self, child: &str) -> Result<Option<PendingChildQuestion>> {
        host_api::questions::pending_child(&self.api, child).map_err(Into::into)
    }

    /// Answer a direct child's pending question as the host user.
    pub async fn answer_child_question(
        &self,
        parent: &str,
        child: &str,
        question_id: &str,
        text: &str,
    ) -> std::result::Result<(), ChildQuestionAnswerError> {
        dispatch(
            &self.api,
            Profile::Full,
            "question.answer_child",
            json!({"parent":parent,"child":child,"question_id":question_id,"text":text}),
        )
        .await
        .map(|_| ())
        .map_err(|error| {
            let error = error.into_inner();
            match error.downcast::<ChildQuestionAnswerError>() {
                Ok(error) => error,
                Err(error) => ChildQuestionAnswerError::Internal(error),
            }
        })
    }
}

/// Wires a store, a provider registry and the usual host capabilities into a
/// [`Host`].
///
/// [`HostBuilder::build`] must be called inside a Tokio runtime context,
/// because the harness it creates spawns its runs there.
pub struct HostBuilder {
    store: Store,
    providers: ProviderRegistry,
    workspace: Option<PathBuf>,
    tools: Option<ToolRegistry>,
    history_tools: bool,
    todo: bool,
    questions: Option<Arc<QuestionTools>>,
    code_mode: Option<PathBuf>,
    wrap: Option<Wrapper>,
    summary: Option<SummaryConfig>,
    context_wrap: Option<ContextWrapper>,
    config: HarnessConfig,
}

impl HostBuilder {
    /// A builder over a store and a provider registry.
    pub fn new(store: Store, providers: ProviderRegistry) -> Self {
        Self {
            store,
            providers,
            workspace: None,
            tools: None,
            history_tools: false,
            todo: false,
            questions: None,
            code_mode: None,
            wrap: None,
            summary: None,
            context_wrap: None,
            config: HarnessConfig::default(),
        }
    }

    /// The workspace to bind the store to. It is created when missing.
    pub fn workspace(mut self, path: impl Into<PathBuf>) -> Self {
        self.workspace = Some(path.into());
        self
    }

    /// The embedder's own model-facing tools. The default is an empty registry.
    pub fn tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = Some(tools);
        self
    }

    /// Register the session archive tools (`history_search`, `history_read`,
    /// `history_chapters`).
    pub fn history_tools(mut self, enabled: bool) -> Self {
        self.history_tools = enabled;
        self
    }

    /// Register the `todo` tool: the model keeps one small list per chat as
    /// host state, read with [`Host::todo`] (see [`crate::todo`]).
    pub fn todo(mut self, enabled: bool) -> Self {
        self.todo = enabled;
        self
    }

    /// Share a question service with the host and register its model tool.
    /// Use the same store as this builder. Hosts with child orchestration should
    /// also bind this service to their agent manager.
    pub fn questions(mut self, questions: Arc<QuestionTools>) -> Self {
        self.questions = Some(questions);
        self
    }

    /// Create the artifact store and the JavaScript adapter under
    /// `artifacts_dir`, and register the adapter.
    pub fn code_mode(mut self, artifacts_dir: impl Into<PathBuf>) -> Self {
        self.code_mode = Some(artifacts_dir.into());
        self
    }

    /// Wrap every history, todo, question and code-mode host tool before registration.
    /// Shown-always status is applied after wrapping and is kept.
    pub fn wrap_support_tools(
        mut self,
        wrap: impl Fn(Arc<dyn Tool>) -> Arc<dyn Tool> + Send + Sync + 'static,
    ) -> Self {
        self.wrap = Some(Arc::new(wrap));
        self
    }

    /// Use readable summarization over the store and the provider registry.
    pub fn summarize(mut self, config: SummaryConfig) -> Self {
        self.summary = Some(config);
        self
    }

    /// Wrap the summarizing context in the embedder's own preparation hook.
    /// Needs [`HostBuilder::summarize`].
    pub fn context(
        mut self,
        wrap: impl Fn(SummarizingContext) -> Arc<dyn ContextPreparation> + Send + Sync + 'static,
    ) -> Self {
        self.context_wrap = Some(Arc::new(wrap));
        self
    }

    /// The harness configuration. The builder fills `context` from
    /// [`HostBuilder::summarize`] and [`HostBuilder::context`]; everything else
    /// is the embedder's.
    pub fn config(mut self, config: HarnessConfig) -> Self {
        self.config = config;
        self
    }

    /// Build the host. Fails when the workspace is missing, when a support tool
    /// cannot be registered, or when the registry exposes no model tool for the
    /// configured execution mode.
    ///
    /// Call this inside a Tokio runtime context.
    pub fn build(self) -> Result<Host> {
        ensure!(
            tokio::runtime::Handle::try_current().is_ok(),
            "HostBuilder::build must be called inside a Tokio runtime context"
        );
        let Self {
            store,
            providers,
            workspace,
            tools,
            history_tools,
            todo: todo_tool,
            questions,
            code_mode,
            wrap,
            summary,
            context_wrap,
            mut config,
        } = self;
        let workspace = workspace.context("HostBuilder::workspace is required")?;
        std::fs::create_dir_all(&workspace)
            .with_context(|| format!("cannot create the workspace {}", workspace.display()))?;
        store
            .bind_workspace(&workspace)
            .context("cannot bind the store to the workspace")?;
        let providers = Arc::new(providers);
        let provider: Arc<dyn Provider> = providers.clone();
        let mut registry = tools.unwrap_or_default();

        let mut support = ToolRegistry::new();
        if history_tools {
            register_history_tools(&mut support, store.clone())
                .context("cannot register the history tools")?;
        }
        if todo_tool {
            todo::register(&mut support, store.clone()).context("cannot register the todo tool")?;
        }
        if let Some(questions) = &questions {
            questions
                .register(&mut support)
                .context("cannot register the question tool")?;
        }
        let mut artifact_store = None;
        let code_mode = match code_mode {
            Some(artifacts_dir) => {
                let artifacts = Arc::new(
                    ArtifactStore::new(&workspace, artifacts_dir)
                        .context("cannot open artifact storage")?,
                );
                let code_mode = Arc::new(
                    CodeMode::new(workspace.clone(), artifacts.clone())
                        .context("cannot start the JavaScript adapter")?,
                );
                artifact_store = Some(artifacts);
                code_mode
                    .register(&mut support)
                    .context("cannot register the JavaScript adapter")?;
                Some(code_mode)
            }
            None => None,
        };
        let wrap: Wrapper = wrap.unwrap_or_else(|| Arc::new(|tool: Arc<dyn Tool>| tool));
        for definition in support.capabilities() {
            let name = definition.name;
            let Some(tool) = support.get_host(&name) else {
                continue;
            };
            let shown_always = support.is_shown_always(&name);
            registry
                .register_host(wrap(tool))
                .with_context(|| format!("cannot register the host tool {name}"))?;
            if shown_always {
                registry.show_always([name]);
            }
        }
        for definition in support.definitions() {
            if let Some(tool) = support.get_model(&definition.name) {
                registry
                    .register(tool)
                    .with_context(|| format!("cannot register the tool {}", definition.name))?;
            }
        }

        match summary {
            Some(summary) => {
                let notes = code_mode
                    .clone()
                    .map(|repl| Arc::new(RuntimeNotes::without_processes(repl)));
                let summary =
                    SummarizingContext::new(store.clone(), provider.clone(), summary, notes)
                        .context("cannot configure summarization")?;
                config.context = match context_wrap {
                    Some(wrap) => wrap(summary),
                    None => Arc::new(summary),
                };
            }
            None => ensure!(
                context_wrap.is_none(),
                "HostBuilder::context needs HostBuilder::summarize"
            ),
        }

        let effective = registry.with_execution_mode(config.execution_mode);
        ensure!(
            !effective.definitions().is_empty(),
            "The tool registry exposes no model tool for {:?} execution mode; add tools with HostBuilder::tools, or HostBuilder::code_mode for JavaScript mode",
            config.execution_mode
        );

        let tools = registry.clone();
        let harness = Arc::new(Harness::new(store.clone(), provider, registry, config));
        if let Some(questions) = &questions {
            questions.set_child_answer_harness(harness.clone())?;
        }
        let mut api = HostCtx::new(harness.clone(), store.clone(), workspace.clone());
        api.read_scope = ReadScope::Workspace(workspace);
        api.repl = code_mode.clone();
        api.artifacts = artifact_store;
        api.questions = questions;
        Ok(Host {
            harness,
            store,
            providers,
            code_mode,
            tools,
            api,
        })
    }
}
