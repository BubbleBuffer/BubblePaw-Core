//! Serving-view artifacts: per-compaction records and the final view.
use super::*;

impl ReplayRun {
    /// Record the view a compaction persisted: a compact snapshot or a dumped view file.
    pub(super) fn record_compaction(&mut self, request: &ModelRequest) -> Result<()> {
        let _phase = self.progress.phase("view_artifacts", self.history.len());
        let view = self
            .store
            .context_view(&self.session_id)?
            .context("Compaction did not persist a view")?;
        if self.options.capture_turn_frontiers {
            self.compactions.push(json!({"step":self.history.len(),"frontier":view.frontier,"frontier_message_id":self.store.context_view_frontier_message_id(&self.session_id,view.frontier)?,"tokens":view.tokens,"budget":view.budget,"threshold":self.threshold,"artifact_basis":"Compact planner snapshot; full serving-view dumps omitted"}));
        } else {
            let file = format!("view-{:03}.txt", self.compactions.len() + 1);
            let mut text = format!("[instructions]\n{}\n", request.system_prompt);
            let mut items =
                vec![json!({"item":"instructions","tokens":token_count(&request.system_prompt)?})];
            for item in &request.messages {
                let kind = if view.prefix.iter().any(|m| m.id == item.id)
                    && self.history.iter().any(|m| m.id == item.id)
                {
                    "latest_typed_request"
                } else if self.history.iter().any(|m| m.id == item.id) {
                    "raw"
                } else if item.content.starts_with("Subject chapter index") {
                    "chapter_index"
                } else if item.content.starts_with("Closed chapter summaries") {
                    "closed_chapter_summaries"
                } else if item.content.starts_with("Open chapter episodes") {
                    "open_episode_summaries"
                } else if item.content.contains("history_search") {
                    "retrieval_instruction"
                } else {
                    "runtime_note_or_legacy"
                };
                text.push_str(&format!(
                    "\n[{} {:?}]\n{}\n",
                    item.id, item.role, item.content
                ));
                if !item.tool_calls.is_empty() {
                    text.push_str(&serde_json::to_string(&item.tool_calls)?);
                    text.push('\n');
                }
                items.push(json!({"item":kind,"message_id":item.id,"tokens":self.estimator.message_tokens(item),"content_tokens":token_count(&item.content)?}));
            }
            std::fs::write(self.options.out.join(&file), text)?;
            items
                .push(json!({"item":"request_overhead_and_tools","tokens":token_count("[]")? + 8}));
            self.compactions.push(json!({"step":self.history.len(),"frontier":view.frontier,"frontier_message_id":self.store.context_view_frontier_message_id(&self.session_id, view.frontier)?,"tokens":view.tokens,"budget":view.budget,"threshold":self.threshold,"view_file":file,"items":items,"masked_tool_outputs":view.masked_tool_output_ids.len(),"clipped_tool_calls":view.clipped_tool_call_ids.len(),"masked_tool_output_ids":view.masked_tool_output_ids,"clipped_tool_call_ids":view.clipped_tool_call_ids}));
        }
        Ok(())
    }

    /// Prepare the final view from the full history; it must need no further compaction.
    pub(super) async fn final_view(
        &mut self,
        corpus: &ReplayCorpus,
        #[cfg(test)] prepared_trace: PreparedTrace<'_>,
    ) -> Result<Value> {
        let final_phase = self.progress.phase("worker_compaction", self.history.len());
        let prepared = self
            .estimator
            .prepare_fresh(&self.session_id, self.history_request(corpus))
            .await?;
        ensure!(
            prepared.notice.is_none(),
            "Final view unexpectedly required another compaction"
        );
        #[cfg(test)]
        if let Some(trace) = prepared_trace {
            trace
                .lock()
                .unwrap()
                .push((self.history.len(), prepared.request.clone()));
        }
        drop(final_phase);
        self.final_view_artifact(&prepared.request)
    }

    fn final_view_artifact(&self, request: &ModelRequest) -> Result<Value> {
        let _phase = self.progress.phase("view_artifacts", self.history.len());
        let mut text = format!("[instructions]\n{}\n", request.system_prompt);
        for message in &request.messages {
            text.push_str(&format!(
                "\n[{} {:?}]\n{}\n",
                message.id, message.role, message.content
            ));
            if !message.tool_calls.is_empty() {
                text.push_str(&serde_json::to_string(&message.tool_calls)?);
                text.push('\n');
            }
        }
        if !self.options.capture_turn_frontiers {
            std::fs::write(self.options.out.join("final-view.txt"), text)?;
        }
        Ok(
            json!({"file":if self.options.capture_turn_frontiers {None}else{Some("final-view.txt")},"artifact_basis":if self.options.capture_turn_frontiers {"Full view archives omitted; checkpoint frozen request hashes and compact frontier metrics retained"}else{"Human-readable view archive"},"tokens":self.estimator.estimated_tokens(request),"messages":request.messages.len(),"canonical_messages":self.history.len(),"extra_model_calls":0}),
        )
    }
}
