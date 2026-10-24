//! Explicit idle compaction preserves the chat prefix and refreshes its cold catalog.
use super::*;

impl Harness {
    /// Compact an idle session through the host's context policy. This accepts
    /// no conversational input and invokes no main-model turn or model tool.
    pub async fn compact_session(
        &self,
        session_id: &str,
        focus: Option<&str>,
    ) -> Result<Option<String>> {
        let cancellation = CancellationToken::new();
        {
            let active = self.state.admission_guard(session_id);
            ensure!(
                !self.state.stopping.load(Ordering::Relaxed),
                "Harness is shutting down"
            );
            if let Some(running) = active.get(session_id).filter(|a| a.finished.is_none()) {
                return Err(SessionBusy {
                    session_id: session_id.into(),
                    run_id: running.run_id.clone(),
                }
                .into());
            }
            let mut compacting = self.state.compacting.lock().unwrap();
            ensure!(
                !compacting.contains_key(session_id),
                "Session is compacting"
            );
            self.store.session(session_id)?;
            compacting.insert(session_id.into(), cancellation.clone());
        }
        let _reservation = CompactionReservation {
            state: self.state.clone(),
            session: session_id.into(),
        };
        self.chat_surface(session_id, &RunOptions::default(), true)?;
        let (_, request_provider) = self.provider_binding();
        let mut request = self.request_template(session_id)?;
        request.messages = self.history(session_id)?;
        let prepared = self
            .prepare_with_writer(
                session_id,
                request,
                request_provider,
                PrepareMode::Compact { focus },
                cancellation,
            )
            .await?;
        self.state.release_history(session_id);
        Ok(prepared.notice)
    }
}
