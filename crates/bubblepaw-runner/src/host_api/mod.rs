//! Transport-independent host commands. Adapters own framing and presentation;
//! authorization, admission, validation and effects belong here.
#[cfg(test)]
mod approval_tests;
pub mod approvals;
pub mod artifacts;
pub mod attachments;
pub mod config;
pub mod context;
#[cfg(test)]
mod environment_tests;
pub mod environments;
mod error;
pub mod events;
pub mod fork;
mod params;
mod read_scope;
pub use read_scope::ReadScope;
pub mod processes;
#[cfg(test)]
mod provider_tests;
pub mod providers;
pub mod questions;
#[cfg(test)]
mod read_scope_event_tests;
#[cfg(test)]
mod read_scope_tests;
pub mod rpc;
#[cfg(test)]
mod rpc_tests;
pub mod runs;
mod runtime;
mod services;
mod session_ops;
pub mod sessions;
#[cfg(test)]
mod storage_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod view_tests;
pub mod views;
mod views_cache;

use bubblepaw_core::{ContextStrategy, Harness, RootStoragePolicy, Store};
pub use error::HostError;
pub use params::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};

/// Profiles are cumulative. Full additionally permits host configuration,
/// process and environment controls, session deletion and shutdown.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    Read,
    Edit,
    Full,
}

#[derive(Clone)]
pub struct HostCtx {
    pub harness: Arc<Harness>,
    pub store: Store,
    pub workspace: PathBuf,
    pub read_scope: ReadScope,
    pub scope: crate::session_scope::SessionScope,
    pub context_strategy: ContextStrategy,
    pub storage_policy: Option<RootStoragePolicy>,
    /// One-shot CLI requests keep the root leased through their cleanup.
    pub hold_admission: bool,
    pub input_limits: crate::input_files::InputLimits,
    pub usage: crate::usage::UsageSettings,
    /// The adapter supplies its resolved launch metadata, never credentials.
    pub config: Value,
    pub providers: Option<crate::provider_setup::ProviderSetup>,
    /// Active runtime metadata remains readable when the disk config is invalid.
    pub provider_config: Option<Arc<crate::provider::LiveProviderConfig>>,
    pub live_config: Option<Arc<crate::live_config::LiveConfig>>,
    pub agents: Option<Arc<crate::agents::AgentTools>>,
    pub repl: Option<Arc<crate::codemode::CodeMode>>,
    pub artifacts: Option<Arc<crate::artifacts::ArtifactStore>>,
    pub input_files: Option<Arc<crate::input_files::InputFiles>>,
    pub processes: Option<Arc<crate::tools::ProcessTools>>,
    pub questions: Option<Arc<crate::question::QuestionTools>>,
    pub wake: Option<Arc<crate::wake::WakeCoordinator>>,
    pub views: Option<Arc<std::sync::Mutex<views::ServingView>>>,
}

impl HostCtx {
    /// Store-only commands need neither credentials nor tool registration.
    pub fn for_store(store: Store, workspace: impl Into<PathBuf>) -> Self {
        let harness = Harness::new(
            store.clone(),
            Arc::new(crate::provider::ScriptedProvider::new("")),
            Default::default(),
            Default::default(),
        );
        Self::new(Arc::new(harness), store, workspace)
    }
    pub fn new(harness: Arc<Harness>, store: Store, workspace: impl Into<PathBuf>) -> Self {
        let workspace = workspace.into();
        Self {
            harness,
            store,
            read_scope: ReadScope::Workspace(workspace.clone()),
            workspace,
            scope: crate::session_scope::SessionScope::all(),
            context_strategy: ContextStrategy::default(),
            storage_policy: None,
            hold_admission: false,
            input_limits: Default::default(),
            usage: Default::default(),
            config: json!({}),
            providers: None,
            provider_config: None,
            live_config: None,
            agents: None,
            repl: None,
            artifacts: None,
            input_files: None,
            processes: None,
            questions: None,
            wake: None,
            views: None,
        }
    }
    pub(crate) fn active_providers(&self) -> Option<&Arc<crate::provider::LiveProviderConfig>> {
        self.provider_config.as_ref().or_else(|| {
            self.providers
                .as_ref()
                .and_then(|setup| setup.0.live.as_ref())
        })
    }
    pub(crate) fn usage_settings(&self) -> crate::usage::UsageSettings {
        match self.active_providers() {
            Some(active) => {
                let generation = active.snapshot();
                // Typed settings retain frozen LCM values when disk requests a restart.
                let mut usage = crate::usage::UsageSettings::from_settings(&generation.settings);
                usage.codex_models_cache = generation.effective.codex_models_cache();
                usage
            }
            None => self.usage.clone(),
        }
    }
    pub fn control(&self) -> crate::session_control::SessionControl {
        crate::session_control::SessionControl::new(
            self.harness.clone(),
            self.store.clone(),
            self.workspace.clone(),
        )
        .with_live_config(self.live_config.clone())
        .with_questions(self.questions.clone())
        .with_agents(self.agents.clone())
        .with_wake(self.wake_coordinator())
    }
    pub fn wake_coordinator(&self) -> Option<Arc<crate::wake::WakeCoordinator>> {
        self.wake
            .clone()
            .or_else(|| self.agents.as_ref().map(|agents| agents.wake_coordinator()))
            .or_else(|| {
                self.processes
                    .as_ref()
                    .map(|processes| processes.wake_coordinator())
            })
    }
    pub fn ensure_session(&self, id: &str) -> Result<(), HostError> {
        self.control().ensure_workspace(id).map_err(Into::into)
    }
    pub fn ensure_read_session(&self, id: &str) -> Result<(), HostError> {
        self.read_scope.ensure(&self.store, id)
    }
}

macro_rules! methods {
    ($($variant:ident => ($name:literal, $profile:ident, $group:ident, $feature:expr)),* $(,)?) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub(crate) enum MethodId { $($variant),* }
        impl MethodId {
            pub fn name(self) -> &'static str { match self { $(Self::$variant => $name,)* } }
            pub fn profile(self) -> Profile { match self { $(Self::$variant => Profile::$profile,)* } }
            fn group(self) -> Group { match self { $(Self::$variant => Group::$group,)* } }
        }
        pub static METHODS: &[Method] = &[$(Method { name: $name, profile: Profile::$profile, feature: $feature, handler: |ctx, profile, params| Box::pin(invoke(ctx,profile,MethodId::$variant,params)) }),*];
    };
}
#[derive(Clone, Copy)]
enum Group {
    Sessions,
    Runtime,
    Services,
    Events,
    Views,
}
pub type Handler = for<'a> fn(
    &'a HostCtx,
    Profile,
    Value,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Value, HostError>> + Send + 'a>,
>;
pub struct Method {
    pub name: &'static str,
    pub profile: Profile,
    pub feature: Option<&'static str>,
    pub handler: Handler,
}
methods! {
    Initialize => ("initialize", Read, Services, None),
    ConfigGet => ("config.get", Read, Services, None),
    ConfigReload => ("config.reload", Full, Services, Some("config_reload")),
    ProvidersList => ("providers.list", Read, Services, Some("providers.list")),
    ProvidersSave => ("providers.save", Full, Services, None),
    ProvidersRemove => ("providers.remove", Full, Services, None),
    ProvidersTest => ("providers.test", Full, Services, None),
    ProvidersReload => ("providers.reload", Full, Services, None),
    RolesSetMain => ("roles.set_main", Full, Services, Some("roles.set_main")),
    SessionOpen => ("session.open", Edit, Sessions, Some("sessions")),
    SessionList => ("session.list", Read, Sessions, Some("session.list.pagination")),
    SessionView => ("session.view", Read, Views, None),
    SessionSidebar => ("session.sidebar", Read, Views, None),
    SessionRename => ("session.rename", Edit, Runtime, None),
    SessionRewind => ("session.rewind", Edit, Runtime, Some("rewind")),
    SessionCompact => ("session.compact", Edit, Runtime, Some("compact")),
    SessionDelete => ("session.delete", Full, Runtime, None),
    SessionArchive => ("session.archive", Edit, Sessions, None),
    SessionUnarchive => ("session.unarchive", Edit, Sessions, None),
    SessionSearch => ("session.search", Read, Sessions, None),
    SessionFork => ("session.fork", Edit, Sessions, None),
    SessionState => ("session.state", Read, Sessions, None),
    SessionHistory => ("session.history", Read, Sessions, None),
    SessionImport => ("session.import", Full, Sessions, None),
    TodoList => ("todo.list", Read, Sessions, Some("todo")),
    QuestionPendingChild => ("question.pending_child", Read, Sessions, Some("child_events")),
    HostCommand => ("host.command", Edit, Runtime, None),
    EventsAfter => ("events.after", Read, Events, Some("events")),
    ContextView => ("context.view", Read, Sessions, Some("context")),
    ContextStructure => ("context.structure", Read, Sessions, None),
    ContextSummary => ("context.summary", Read, Sessions, None),
    ContextEvents => ("context.events", Read, Sessions, None),
    HistoryRange => ("history.range", Read, Sessions, None),
    HistoryMessages => ("history.messages", Read, Sessions, None),
    EnvironmentInspect => ("environment.inspect", Read, Services, Some("environments")),
    EnvironmentControl => ("environment.control", Full, Services, Some("codemode")),
    RunStart => ("run.start", Edit, Runtime, Some("tools")),
    RunResume => ("run.resume", Edit, Runtime, Some("paragraphs")),
    RunSteer => ("run.steer", Edit, Runtime, Some("steer")),
    RunFollowUp => ("run.follow_up", Edit, Runtime, Some("follow_up")),
    RunCancel => ("run.cancel", Edit, Runtime, Some("cancel")),
    RunWait => ("run.wait", Read, Runtime, None),
    ChildCancel => ("child.cancel", Edit, Runtime, Some("child.cancel")),
    ProcessBackground => ("process.background", Full, Services, None),
    ProcessList => ("process.list", Read, Services, Some("processes")),
    ApprovalList => ("approval.list", Read, Services, Some("approvals")),
    ApprovalDecide => ("approval.decide", Edit, Services, None),
    QuestionAnswer => ("question.answer", Edit, Runtime, Some("question")),
    QuestionAnswerChild => ("question.answer_child", Edit, Runtime, Some("question.answer_child")),
    AttachmentsAdd => ("attachments.add", Edit, Services, None),
    AttachmentsRead => ("attachments.read", Read, Services, None),
    ArtifactsList => ("artifacts.list", Read, Services, Some("outputs")),
    ArtifactsRead => ("artifacts.read", Read, Services, None),
    StorageStats => ("storage.stats", Read, Services, None),
    Shutdown => ("shutdown", Full, Services, None),
}

/// Dispatch the same method table for every transport. Profile checks precede
/// parameter decoding and service lookup, so unavailable services cannot bypass it.
pub async fn dispatch(
    ctx: &HostCtx,
    profile: Profile,
    name: &str,
    params: Value,
) -> Result<Value, HostError> {
    let method = METHODS
        .iter()
        .find(|method| method.name == name)
        .ok_or_else(|| HostError::unknown(name))?;
    if profile < method.profile {
        return Err(HostError::denied(name));
    }
    (method.handler)(ctx, profile, params).await
}
async fn invoke(
    ctx: &HostCtx,
    profile: Profile,
    method: MethodId,
    params: Value,
) -> Result<Value, HostError> {
    if profile < method.profile() {
        return Err(HostError::denied(method.name()));
    }
    match method.group() {
        Group::Runtime => runtime::command(ctx, profile, method, params).await,
        Group::Services => services::command(ctx, method, params).await,
        Group::Sessions => {
            let ctx = ctx.clone();
            tokio::task::spawn_blocking(move || sessions::command(&ctx, method, params))
                .await
                .map_err(|e| HostError::internal(e.to_string()))?
        }
        Group::Events => {
            let ctx = ctx.clone();
            tokio::task::spawn_blocking(move || events::after(&ctx, params))
                .await
                .map_err(|e| HostError::internal(e.to_string()))?
        }
        Group::Views => {
            let ctx = ctx.clone();
            tokio::task::spawn_blocking(move || views::command(&ctx, method, params))
                .await
                .map_err(|e| HostError::internal(e.to_string()))?
        }
    }
}

pub(crate) fn required<'a, T>(service: &'a Option<Arc<T>>, name: &str) -> Result<&'a T, HostError> {
    service
        .as_deref()
        .ok_or_else(|| HostError::unavailable(format!("{name} not available in this host")))
}
pub(crate) fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, HostError> {
    serde_json::from_value(value).map_err(|e| HostError::invalid(e.to_string()))
}
pub(crate) fn encode(value: impl Serialize) -> Result<Value, HostError> {
    serde_json::to_value(value).map_err(|e| HostError::internal(e.to_string()))
}
