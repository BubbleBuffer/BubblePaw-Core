//! Per-section settings owners. Each live section has one owner that publishes it
//! whole on a `tokio::sync::watch` channel; components subscribe to their own
//! section and read it when they act, so a key is live when its consumer
//! subscribes. Validation happens before publication (`prepare`, `McpSettings::validate`).
use crate::{
    config::{Agents, ApprovalSettings, ExecConfig, Settings, SkillsConfig, WebSettings},
    toolsets::ToolsetSettings,
};
use bubblepaw_mcp::McpSettings;
use std::sync::Arc;
use tokio::sync::watch;

/// One section's owner: publishes whole values, only when they changed.
pub struct Section<T> {
    tx: watch::Sender<Arc<T>>,
}
impl<T: PartialEq> Section<T> {
    pub fn new(initial: T) -> Self {
        Self {
            tx: watch::channel(Arc::new(initial)).0,
        }
    }
    pub fn subscribe(&self) -> watch::Receiver<Arc<T>> {
        self.tx.subscribe()
    }
    pub fn current(&self) -> Arc<T> {
        self.tx.borrow().clone()
    }
    /// Replace the section; returns whether subscribers were notified.
    pub fn publish(&self, value: T) -> bool {
        self.tx.send_if_modified(|current| {
            if **current == value {
                return false;
            }
            *current = Arc::new(value);
            true
        })
    }
}

/// The live context controls: capacity, compaction threshold and automatic preparation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextLive {
    pub window_tokens: Option<usize>,
    pub compact_threshold_tokens: usize,
    pub auto_summarize: bool,
}
impl From<&Settings> for ContextLive {
    fn from(settings: &Settings) -> Self {
        Self {
            window_tokens: settings.context_window_tokens,
            compact_threshold_tokens: settings.compact_threshold_tokens,
            auto_summarize: settings.auto_summarize,
        }
    }
}

/// Owners of the sections published from the accepted settings. The provider
/// generation (model, roles, providers) is owned by `LiveProviderConfig`.
pub struct SettingsHub {
    context: Section<ContextLive>,
    mcp: Section<McpSettings>,
    approvals: Section<ApprovalSettings>,
    exec: Section<ExecConfig>,
    agents: Section<Agents>,
    web: Section<WebSettings>,
    skills: Section<SkillsConfig>,
    toolsets: Section<ToolsetSettings>,
}
impl SettingsHub {
    pub fn new(settings: &Settings) -> Self {
        Self {
            context: Section::new(settings.into()),
            mcp: Section::new(settings.mcp.clone()),
            approvals: Section::new(settings.approvals.clone()),
            exec: Section::new(settings.exec.clone()),
            agents: Section::new(settings.agents.clone()),
            web: Section::new(settings.web.clone()),
            skills: Section::new(settings.skills.clone()),
            toolsets: Section::new(settings.into()),
        }
    }
    pub fn context(&self) -> watch::Receiver<Arc<ContextLive>> {
        self.context.subscribe()
    }
    pub fn mcp(&self) -> watch::Receiver<Arc<McpSettings>> {
        self.mcp.subscribe()
    }
    pub fn exec(&self) -> watch::Receiver<Arc<ExecConfig>> {
        self.exec.subscribe()
    }
    pub fn agents(&self) -> watch::Receiver<Arc<Agents>> {
        self.agents.subscribe()
    }
    pub fn approvals(&self) -> Arc<ApprovalSettings> {
        self.approvals.current()
    }
    pub fn web(&self) -> Arc<WebSettings> {
        self.web.current()
    }
    pub fn skills(&self) -> Arc<SkillsConfig> {
        self.skills.current()
    }
    pub fn toolsets(&self) -> Arc<ToolsetSettings> {
        self.toolsets.current()
    }
    /// Publish validated settings; returns the sections that changed.
    pub fn publish(&self, settings: &Settings) -> Vec<&'static str> {
        let mut changed = Vec::new();
        if self.context.publish(settings.into()) {
            changed.push("context");
        }
        if self.mcp.publish(settings.mcp.clone()) {
            changed.push("mcp");
        }
        if self.approvals.publish(settings.approvals.clone()) {
            changed.push("approvals");
        }
        if self.exec.publish(settings.exec.clone()) {
            changed.push("exec");
        }
        if self.agents.publish(settings.agents.clone()) {
            changed.push("agents");
        }
        if self.web.publish(settings.web.clone()) {
            changed.push("web");
        }
        if self.skills.publish(settings.skills.clone()) {
            changed.push("skills");
        }
        if self.toolsets.publish(settings.into()) {
            changed.push("toolsets");
        }
        changed
    }
}
