//! One replay message: import, worker catch-up, context preparation and snapshots.
use super::*;
use std::ops::ControlFlow;

impl ReplayRun {
    /// Request for the committed history; the session id stays empty, as recorded.
    pub(super) fn history_request(&self, corpus: &ReplayCorpus) -> ModelRequest {
        ModelRequest {
            output_schema: None,
            reasoning_effort: None,
            model: corpus
                .recorded_model
                .clone()
                .unwrap_or_else(|| "recorded-unknown".into()),
            system_prompt: corpus.instructions.clone(),
            messages: self.history.clone(),
            tools: vec![],
            disabled_tools: vec![],
            max_output_tokens: None,
            session_id: String::new(),
            ..Default::default()
        }
    }

    pub(super) async fn step(
        &mut self,
        corpus: &ReplayCorpus,
        message: &Message,
        #[cfg(test)] prepared_trace: PreparedTrace<'_>,
    ) -> Result<ControlFlow<()>> {
        if self.telemetry.lock().unwrap().guard().is_err() {
            self.imports.flush_profiled(
                &mut self.importer,
                &self.progress,
                self.compactions.len(),
            )?;
            self.stopped = self
                .telemetry
                .lock()
                .unwrap()
                .stopped
                .clone()
                .or(Some("Spend limit".into()));
            return Ok(ControlFlow::Break(()));
        }
        self.import_message(message)?;
        if self.catch_up_worker().await?.is_break() {
            return Ok(ControlFlow::Break(()));
        }
        let (turn, checkpoint) = self.locate_checkpoint();
        if (self.active_tokens < self.threshold || !self.pending_calls.is_empty())
            && (checkpoint.is_none() || !self.pending_calls.is_empty())
        {
            self.snapshot_incomplete_round(turn, checkpoint)?;
            return Ok(ControlFlow::Continue(()));
        }
        // Most imported messages need no context preparation. Avoid copying
        // the growing transcript until compaction or a probe checkpoint needs it.
        self.imports
            .flush_profiled(&mut self.importer, &self.progress, self.compactions.len())?;
        let preparation_phase = self.progress.phase("worker_compaction", self.history.len());
        let request = self.history_request(corpus);
        let should_compact = self.active_tokens >= self.threshold;
        let active = self
            .estimator
            .prepare_step(
                &self.session_id,
                request,
                should_compact,
                &self.context_state,
            )
            .await;
        drop(preparation_phase);
        // Preparation can persist a checkpoint/coverage before a fit error.
        self.worker_gate.refresh(
            &self.store,
            &self.session_id,
            &self.history,
            self.options.context_strategy,
        )?;
        let Ok(prepared) = active else {
            return self.preparation_failed(turn, checkpoint);
        };
        #[cfg(test)]
        if let Some(trace) = prepared_trace {
            trace
                .lock()
                .unwrap()
                .push((self.history.len(), prepared.request.clone()));
        }
        if should_compact {
            let _phase = self
                .progress
                .phase("masking_token_estimation", self.history.len());
            self.active_tokens = self.estimator.estimated_tokens(&prepared.request);
        }
        if is_typed_user(message)
            && let Some(turn) = self.view_tokens_per_turn.last_mut()
        {
            turn["tokens"] = json!(self.active_tokens);
        }
        if prepared.notice.is_some() {
            self.record_compaction(&prepared.request)?;
        }
        self.progress.counts(
            self.imports.committed_messages,
            self.compactions.len(),
            self.imports.committed_transactions,
        );
        if should_compact {
            self.progress.write(
                "running",
                self.imports.committed_messages,
                self.compactions.len(),
            )?;
        }
        if let Some(turn) = turn
            && (self.options.capture_turn_frontiers || checkpoint.is_some())
        {
            let mut snapshot = turn_snapshot(
                &self.store,
                &self.session_id,
                &self.history,
                turn,
                self.compactions.len(),
                self.active_tokens,
                false,
            )?;
            snapshot["checkpoint"] = json!(checkpoint);
            self.turn_frontiers.push(snapshot);
        }
        if let Some(checkpoint) = checkpoint {
            return self
                .run_checkpoint(turn, checkpoint, &prepared.request)
                .await;
        }
        Ok(ControlFlow::Continue(()))
    }

    fn import_message(&mut self, message: &Message) -> Result<()> {
        {
            let _phase = self
                .progress
                .phase("canonical_serialization", self.history.len() + 1);
            self.imports.push(message)?;
        }
        self.history.push(message.clone());
        if !self.execution.optimized()
            || self.imports.is_full()
            || self
                .history
                .len()
                .is_multiple_of(PROGRESS_INTERVAL_MESSAGES)
        {
            self.imports.flush_profiled(
                &mut self.importer,
                &self.progress,
                self.compactions.len(),
            )?;
        }
        if self
            .history
            .len()
            .is_multiple_of(PROGRESS_INTERVAL_MESSAGES)
        {
            self.progress.write(
                "running",
                self.imports.committed_messages,
                self.compactions.len(),
            )?;
        }
        {
            let _phase = self
                .progress
                .phase("masking_token_estimation", self.history.len());
            let tokens = self.estimator.message_tokens(message);
            self.worker_gate.observe(message, tokens)?;
            self.active_tokens += tokens;
        }
        if is_typed_user(message) {
            self.view_tokens_per_turn
                .push(json!({"message_id":message.id,"step":self.history.len(),"tokens":self.active_tokens}));
        }
        for call in &message.tool_calls {
            self.pending_calls.insert(call.id.clone());
        }
        if let Some(id) = &message.tool_call_id {
            self.pending_calls.remove(id);
        }
        self.activated |= self.active_tokens >= self.threshold.div_ceil(2);
        Ok(())
    }

    /// Catch the episode worker up when the gate says a step is due.
    async fn catch_up_worker(&mut self) -> Result<ControlFlow<()>> {
        let ready = {
            let _phase = self.progress.phase("worker_compaction", self.history.len());
            if !self.activated {
                Ok(false)
            } else if !self.execution.optimized() {
                Ok(true)
            } else if self.options.context_strategy == ContextStrategy::Classical {
                Ok(self.worker_gate.needs_classical())
            } else {
                self.worker_gate
                    .needs_lcm(&self.history, &self.pending_calls)
            }
        };
        let worker_result = match ready {
            Ok(false) => Ok(()),
            Ok(true) => {
                self.imports.flush_profiled(
                    &mut self.importer,
                    &self.progress,
                    self.compactions.len(),
                )?;
                let _phase = self.progress.phase("worker_compaction", self.history.len());
                let result = if self.options.context_strategy == ContextStrategy::Classical {
                    self.worker.precompute(&self.session_id).await
                } else {
                    self.worker
                        .catch_up_until(&self.session_id, self.history.len())
                        .await
                };
                self.worker_gate.refresh(
                    &self.store,
                    &self.session_id,
                    &self.history,
                    self.options.context_strategy,
                )?;
                result
            }
            Err(error) => {
                self.imports.flush_profiled(
                    &mut self.importer,
                    &self.progress,
                    self.compactions.len(),
                )?;
                Err(error)
            }
        };
        if worker_result.is_err() {
            self.failures.push(json!({"step":self.history.len(),"phase":"worker","error":"Episode worker model or coverage failure"}));
            self.stopped = self.telemetry.lock().unwrap().stopped.clone().or(Some(
                "Episode operation failed after its bounded retry policy".into(),
            ));
            return Ok(ControlFlow::Break(()));
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Typed-user turn ending at this step, and the checkpoint planned for it.
    fn locate_checkpoint(&self) -> (Option<usize>, Option<usize>) {
        // Preserve the per-message worker timeline: skip only planner-proven
        // no-ops. Every real operation observes exactly this committed prefix.
        // Main-model preparation still waits for complete tool rounds.
        let turn = self
            .turn_steps
            .iter()
            .position(|step| *step == self.history.len())
            .map(|index| index + 1);
        let checkpoint = turn
            .and_then(|turn| {
                self.options
                    .checkpoints_at_turns
                    .iter()
                    .position(|position| *position == turn)
            })
            .map(|index| index + 1);
        (turn, checkpoint)
    }

    fn snapshot_incomplete_round(
        &mut self,
        turn: Option<usize>,
        checkpoint: Option<usize>,
    ) -> Result<()> {
        if let Some(turn) = turn
            && (self.options.capture_turn_frontiers || checkpoint.is_some())
        {
            self.imports.flush_profiled(
                &mut self.importer,
                &self.progress,
                self.compactions.len(),
            )?;
            let snapshot = turn_snapshot(
                &self.store,
                &self.session_id,
                &self.history,
                turn,
                self.compactions.len(),
                self.active_tokens,
                !self.pending_calls.is_empty(),
            )?;
            if let Some(checkpoint) = checkpoint {
                self.plan_mismatches.push(json!({"checkpoint":checkpoint,"turn":turn,"reason":"incomplete_tool_round","actual":snapshot}));
            }
            self.turn_frontiers.push(snapshot);
        }
        Ok(())
    }

    fn preparation_failed(
        &mut self,
        turn: Option<usize>,
        checkpoint: Option<usize>,
    ) -> Result<ControlFlow<()>> {
        self.failures.push(json!({"step":self.history.len(),"phase":"compaction","error":"Real compaction could not fit or catch up"}));
        if let Some(reason) = &self.telemetry.lock().unwrap().stopped {
            self.stopped = Some(reason.clone());
            return Ok(ControlFlow::Break(()));
        }
        if let Some(turn) = turn
            && (self.options.capture_turn_frontiers || checkpoint.is_some())
        {
            self.imports.flush_profiled(
                &mut self.importer,
                &self.progress,
                self.compactions.len(),
            )?;
            let mut snapshot = turn_snapshot(
                &self.store,
                &self.session_id,
                &self.history,
                turn,
                self.compactions.len(),
                self.active_tokens,
                false,
            )?;
            snapshot["status"] = json!("preparation_failed");
            if let Some(checkpoint) = checkpoint {
                self.plan_mismatches.push(json!({"checkpoint":checkpoint,"turn":turn,"reason":"preparation_failed","actual":snapshot}));
            }
            self.turn_frontiers.push(snapshot);
        }
        Ok(ControlFlow::Continue(()))
    }
}
