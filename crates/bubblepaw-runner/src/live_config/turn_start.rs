//! The turn-start hook: reports worktree cleanup, then refreshes live config.
use super::*;

pub(super) struct ConfigTurnStart(pub(super) Weak<LiveConfig>);
#[async_trait]
impl TurnStart for ConfigTurnStart {
    async fn before_turn(&self, session: &str, cancellation: CancellationToken) -> Result<()> {
        if let Some(live) = self.0.upgrade() {
            let agents = live.agents.lock().unwrap().upgrade();
            if let Some(agents) = agents {
                agents.report_worktree_cleanup().await;
            }
            live.memory.check(session)?;
            tokio::select! {
                _ = cancellation.cancelled() => return Err(anyhow!("Cancelled during config refresh")),
                _ = live.check() => {},
            }
            live.check_skills(session, &live.providers.snapshot().settings)?;
        }
        Ok(())
    }
}
