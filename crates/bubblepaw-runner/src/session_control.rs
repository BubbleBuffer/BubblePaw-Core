//! Session control shared by every host transport: slash commands, cancel and
//! the workspace binding check. Stdio, web and the CLI are thin adapters over
//! [`SessionControl`], so one command behaves the same on each of them.

use std::{
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use bubblepaw_core::{ClientInfo, Harness, Store};
use serde_json::{Value, json};

use crate::{
    agents::AgentTools, live_config::LiveConfig, question::QuestionTools, wake::WakeCoordinator,
};

/// A host command typed in place of a prompt. It never becomes a
/// conversational message and never starts a model turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlashCommand<'a> {
    /// `/rename <title>`; an empty title is rejected by the executor.
    Rename(&'a str),
    /// `/rewind <message id>`; an empty id is rejected by the executor.
    Rewind(&'a str),
    /// `/compact [focus]`.
    Compact(Option<&'a str>),
    /// `/reload [server]`; host-scoped, it needs no session.
    Reload(Option<&'a str>),
}

impl<'a> SlashCommand<'a> {
    /// Recognize a whole input as a host command. Anything else, including
    /// `/renamed` or `/reload a b`, is an ordinary prompt.
    pub fn parse(text: &'a str) -> Option<Self> {
        if let Some(title) = crate::sessions::rename_command(text) {
            return Some(Self::Rename(title));
        }
        if let Some(message) = crate::rewind::rewind_command(text) {
            return Some(Self::Rewind(message));
        }
        if let Some(focus) = crate::context_strategies::compact_command(text) {
            return Some(Self::Compact(focus));
        }
        crate::live_config::reload_command(text).map(Self::Reload)
    }

    /// Whether the command acts on one session (all but `/reload`).
    pub fn needs_session(self) -> bool {
        !matches!(self, Self::Reload(_))
    }
}

/// The session is bound to a different workspace than this host serves.
/// Web maps it to 409 Conflict.
#[derive(Debug)]
pub struct WorkspaceMismatch(pub PathBuf);

impl fmt::Display for WorkspaceMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Session belongs to another workspace: {}",
            self.0.display()
        )
    }
}

impl std::error::Error for WorkspaceMismatch {}

/// Per-host session control. Optional services are absent in reduced hosts
/// (for example a web router built without agents); their part is skipped.
#[derive(Clone)]
pub struct SessionControl {
    harness: Arc<Harness>,
    store: Store,
    workspace: PathBuf,
    live_config: Option<Arc<LiveConfig>>,
    questions: Option<Arc<QuestionTools>>,
    agents: Option<Arc<AgentTools>>,
    wake: Option<Arc<WakeCoordinator>>,
}

impl SessionControl {
    pub fn new(harness: Arc<Harness>, store: Store, workspace: impl Into<PathBuf>) -> Self {
        Self {
            harness,
            store,
            workspace: workspace.into(),
            live_config: None,
            questions: None,
            agents: None,
            wake: None,
        }
    }
    pub fn with_live_config(mut self, live_config: Option<Arc<LiveConfig>>) -> Self {
        self.live_config = live_config;
        self
    }
    pub fn with_questions(mut self, questions: Option<Arc<QuestionTools>>) -> Self {
        self.questions = questions;
        self
    }
    pub fn with_agents(mut self, agents: Option<Arc<AgentTools>>) -> Self {
        self.agents = agents;
        self
    }
    pub fn with_wake(mut self, wake: Option<Arc<WakeCoordinator>>) -> Self {
        self.wake = wake;
        self
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// A missing session fails as not found (`QueryReturnedNoRows`); a
    /// session bound to another workspace fails with [`WorkspaceMismatch`].
    /// Neither opens or restores the chat.
    pub fn ensure_workspace(&self, session: &str) -> Result<()> {
        self.store.session_root_id(session)?;
        if let Some(bound) = self.store.session_workspace(session)?
            && bound != self.workspace
        {
            return Err(WorkspaceMismatch(bound).into());
        }
        Ok(())
    }

    /// Run `text` as a host command when it is one. `Ok(None)` means it is an
    /// ordinary prompt and the caller submits it as usual.
    pub async fn run_command(&self, session: Option<&str>, text: &str) -> Result<Option<Value>> {
        match SlashCommand::parse(text) {
            Some(command) => self.execute(session, command).await.map(Some),
            None => Ok(None),
        }
    }

    /// Execute one command. Session commands check the workspace binding
    /// first; `/reload` checks it only when a session is given.
    pub async fn execute(&self, session: Option<&str>, command: SlashCommand<'_>) -> Result<Value> {
        if let Some(session) = session {
            self.ensure_workspace(session)?;
        }
        let session_id = || session.with_context(|| "this command needs a session".to_owned());
        Ok(match command {
            SlashCommand::Rename(title) => {
                serde_json::to_value(crate::sessions::rename(&self.store, session_id()?, title)?)?
            }
            SlashCommand::Rewind(message) => {
                let store = self.store.clone();
                let workspace = self.workspace.clone();
                let session = session_id()?.to_owned();
                let message = message.to_owned();
                let result = tokio::task::spawn_blocking(move || {
                    crate::rewind::rewind_session(&store, &workspace, &session, &message)
                })
                .await??;
                serde_json::to_value(result)?
            }
            SlashCommand::Compact(focus) => {
                json!({"notice": self.harness.compact_session(session_id()?, focus).await?})
            }
            SlashCommand::Reload(server) => {
                let Some(live) = &self.live_config else {
                    bail!("config reload is not available in this host");
                };
                json!({"reload": live.reload(server).await})
            }
        })
    }

    /// Cancel a session's run, its pending question and its children, and
    /// hold back automatic completion wakes. Returns whether anything was
    /// cancelled.
    pub async fn cancel(&self, session: &str, client: Option<ClientInfo>) -> Result<bool> {
        self.ensure_workspace(session)?;
        if let Some(wake) = &self.wake {
            wake.suppress(session);
        }
        let question_cancelled = match &self.questions {
            Some(questions) => questions.cancel(session)?,
            None => false,
        };
        let requested = self.harness.cancel_from(session, client);
        if let Some(agents) = &self.agents {
            agents.cancel_children(session).await?;
        }
        Ok(requested || question_cancelled)
    }
}
