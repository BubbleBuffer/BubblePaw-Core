//! What the main chat, child sessions and the host API read from the published registry and skill catalog.
use super::*;
use crate::{
    skills::SkillInfo,
    toolsets::{self, ToolInfo},
};
use bubblepaw_core::HarnessConfig;
use serde::Deserialize;

/// The skills of the host workspace and whether the catalog is on at all.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillList {
    /// `skills.enabled`: when false, no skill reaches the catalog whatever its own switch says.
    pub enabled: bool,
    pub skills: Vec<SkillInfo>,
}

impl LiveConfig {
    /// The registry the main chat runs with: the published one seen through `agent.toolset`.
    pub fn main_registry(&self) -> ToolRegistry {
        self.main_tools.read().unwrap().clone()
    }
    /// `config` with the whole published registry as its tool source. Child sessions
    /// choose their tools by profile, so the main chat's toolset does not narrow them.
    pub fn child_harness_config(self: &Arc<Self>, config: &HarnessConfig) -> HarnessConfig {
        let weak = Arc::downgrade(self);
        let mut child = config.clone();
        child.registry_resolver = Some(Arc::new(move || {
            weak.upgrade()
                .map(|live| live.registry())
                .unwrap_or_default()
        }));
        child
    }
    /// Every published tool with its category and source, whatever the main toolset.
    pub fn tool_list(&self) -> Vec<ToolInfo> {
        toolsets::list(&self.registry())
    }
    /// The skills a session in the host workspace could see, with their switches.
    pub fn skill_list(&self) -> Result<SkillList> {
        let settings = self.providers.hub().skills();
        Ok(SkillList {
            enabled: settings.enabled,
            skills: self.skills.list(&settings)?,
        })
    }
}
