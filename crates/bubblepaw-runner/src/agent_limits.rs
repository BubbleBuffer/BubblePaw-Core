//! The agent limits a registry reads: a fixed value for standalone hosts, or the
//! host's published `agents` section once followed.
use crate::config::Agents;
use std::sync::{Arc, OnceLock, RwLock};
use tokio::sync::watch;

#[derive(Default)]
pub struct AgentLimits {
    fixed: RwLock<Option<Agents>>,
    section: OnceLock<watch::Receiver<Arc<Agents>>>,
}
impl AgentLimits {
    pub fn set(&self, limits: Agents) {
        *self.fixed.write().expect("agent limits lock") = Some(limits);
    }
    pub fn fixed(&self) -> Option<Agents> {
        self.fixed.read().expect("agent limits lock").clone()
    }
    pub fn follow(&self, section: watch::Receiver<Arc<Agents>>) {
        let _ = self.section.set(section);
    }
    pub fn get(&self) -> Agents {
        match self.section.get() {
            Some(section) => (**section.borrow()).clone(),
            None => self.fixed().unwrap_or_default(),
        }
    }
}
