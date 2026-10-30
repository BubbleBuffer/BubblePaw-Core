//! Validated publication of a serving view and its optional chat surface snapshot.
use super::*;

impl Store {
    /// Replace all view data in exactly one transaction after validating its references.
    pub fn save_context_view(&self, view: &ContextView) -> Result<()> {
        self.save_context_view_events(view, &[], None)
    }

    /// Publish a view with an authoritative apply fact in its existing validation transaction.
    pub fn save_context_view_with_event(
        &self,
        view: &ContextView,
        event: &ContextEvent,
    ) -> Result<()> {
        anyhow::ensure!(
            event.kind == ContextEventKind::Applied,
            "View requires an applied event"
        );
        self.save_context_view_events(view, std::slice::from_ref(event), None)
    }

    /// Publish a view and optional host prompt snapshot in the same canonical
    /// transaction. Failed validation or commit leaves the prior surface intact.
    pub fn save_context_view_with_surface(
        &self,
        view: &ContextView,
        event: Option<&ContextEvent>,
        surface: Option<&ContextSurfaceRebind>,
    ) -> Result<()> {
        anyhow::ensure!(
            event.is_none_or(|event| event.kind == ContextEventKind::Applied),
            "View requires an applied event"
        );
        self.save_context_view_events(
            view,
            event.map(std::slice::from_ref).unwrap_or(&[]),
            surface,
        )
    }

    fn save_context_view_events(
        &self,
        view: &ContextView,
        events: &[ContextEvent],
        surface: Option<&ContextSurfaceRebind>,
    ) -> Result<()> {
        if let Some(store) = self.scoped(&view.session_id)? {
            return store.save_context_view_events(view, events, surface);
        }
        anyhow::ensure!(
            !view.session_id.trim().is_empty(),
            "View session ID is empty"
        );
        anyhow::ensure!(
            !view.created_at.trim().is_empty(),
            "View creation time is empty"
        );
        self.write_context_view_events(view, events, |tx| {
            let session = Self::session_from_connection(tx, &view.session_id)?;
            if let Some(frontier) = view.frontier {
                let member: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE session_id=?1 AND seq=?2)", params![view.session_id, frontier], |row| row.get(0))?;
                anyhow::ensure!(member, "View frontier does not belong to the session");
                if session.context_strategy == ContextStrategy::Classical {
                    // A ready summary snapshot may be applied while the worker
                    // advances the latest checkpoint. Its frozen prefix owns
                    // that snapshot's text independently of the latest pointer.
                    let covered: bool = tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM context_checkpoints WHERE session_id=?1 AND through_message_seq>=?2)",
                        params![view.session_id, frontier], |row| row.get(0),
                    )?;
                    let source_boundary = Self::imported_history_seed_at(tx,&view.session_id)?.0;
                    anyhow::ensure!(covered || source_boundary==Some(frontier), "Classical view frontier must be covered by a checkpoint or imported summary");
                    Self::validate_complete_frontier(tx, &view.session_id, frontier)?;
                } else {
                    let boundary: bool = tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM context_episodes WHERE session_id=?1 AND last_message_seq=?2) OR EXISTS(SELECT 1 FROM context_chapters WHERE session_id=?1 AND last_message_seq=?2) OR EXISTS(SELECT 1 FROM context_checkpoints WHERE session_id=?1 AND through_message_seq=?2)",
                        params![view.session_id, frontier], |row| row.get(0),
                    )?;
                    let source_boundary = Self::imported_history_seed_at(tx,&view.session_id)?.0;
                    anyhow::ensure!(boundary || source_boundary==Some(frontier), "View frontier must be an episode, legacy or imported coverage boundary");
                }
            }
            for message_id in &view.masked_tool_output_ids {
                let seq = Self::session_message_seq(tx, &view.session_id, message_id)?;
                anyhow::ensure!(view.frontier.is_none_or(|frontier| seq > frontier), "Masked output must be in the raw zone");
                let output:bool=tx.query_row("SELECT h.role='tool' AND EXISTS(SELECT 1 FROM message_tool_calls WHERE session_id=?1 AND message_seq=?2 AND kind='result') FROM message_headers h WHERE h.session_id=?1 AND h.message_seq=?2",params![view.session_id,seq],|row|row.get(0))?;
                anyhow::ensure!(output, "Masked message must be a tool output");
            }
            if !view.clipped_tool_call_ids.is_empty() {
                for call_id in &view.clipped_tool_call_ids {
                    let member:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM message_tool_calls WHERE session_id=?1 AND call_id=?2 AND message_seq>?3 AND kind='call')",params![view.session_id,call_id,view.frontier.unwrap_or(0)],|row|row.get(0))?;
                    anyhow::ensure!(member, "Clipped tool call must belong to the session raw zone");
                }
            }
            let data = Self::encode_context_view(tx, view)?;
            Self::insert_context_events_tx(tx, &view.session_id, events)?;
            if let Some(surface) = surface {
                Self::rebind_chat_surface_tx(tx, &view.session_id, surface)?;
            }
            Ok(data)
        })
    }
}
