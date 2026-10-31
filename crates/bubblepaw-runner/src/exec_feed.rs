//! Exec settings a process host reads: a fixed value for standalone use, or the
//! host's published `exec` section once followed.
use crate::config::ExecConfig;
use std::sync::{Arc, OnceLock, RwLock};
use tokio::sync::watch;

#[derive(Default)]
pub struct ExecFeed {
    fixed: RwLock<ExecConfig>,
    live: OnceLock<watch::Receiver<Arc<ExecConfig>>>,
}
impl ExecFeed {
    pub fn set(&self, config: ExecConfig) {
        *self.fixed.write().expect("exec config lock") = config;
    }
    pub fn follow(&self, section: watch::Receiver<Arc<ExecConfig>>) {
        let _ = self.live.set(section);
    }
    pub fn get(&self) -> ExecConfig {
        match self.live.get() {
            Some(section) => (**section.borrow()).clone(),
            None => self.fixed.read().expect("exec config lock").clone(),
        }
    }
}
