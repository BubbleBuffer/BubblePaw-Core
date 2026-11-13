//! Probe checkpoints: plan comparison, recall probes and their judged rows.
use super::*;
use crate::probes::Probe;
use std::ops::ControlFlow;

impl ReplayRun {
    pub(super) async fn run_checkpoint(
        &mut self,
        turn: Option<usize>,
        checkpoint: usize,
        request: &ModelRequest,
    ) -> Result<ControlFlow<()>> {
        self.turn_frontiers.last_mut().unwrap()["frozen_view_sha256"] =
            json!(crate::probes::fingerprint(request)?);
        let view = self.store.context_view(&self.session_id)?;
        let frontier = view.as_ref().and_then(|view| view.frontier);
        let frontier_id = self
            .store
            .context_view_frontier_message_id(&self.session_id, frontier)?;
        let mismatch = self.compare_plan(turn, checkpoint, frontier_id.as_deref())?;
        let delivered_workers = self
            .ask_probes(
                turn,
                checkpoint,
                frontier,
                frontier_id.as_deref(),
                mismatch,
                request,
            )
            .await?;
        // Durable result snapshots after each checkpoint; paid answers never
        // enter the source chat; workers retain their rounds until acknowledgement.
        std::fs::write(
            self.options.out.join("probes.json"),
            serde_json::to_vec_pretty(&self.probe_rows)?,
        )?;
        for answer in delivered_workers {
            answer.acknowledge().await?;
        }
        if let Some(reason) = &self.telemetry.lock().unwrap().stopped {
            self.stopped = Some(reason.clone());
            return Ok(ControlFlow::Break(()));
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Compare this checkpoint with the pinned plan; true on a frontier or eligibility mismatch.
    fn compare_plan(
        &mut self,
        turn: Option<usize>,
        checkpoint: usize,
        frontier_id: Option<&str>,
    ) -> Result<bool> {
        let planned = self
            .pinned_plan
            .as_ref()
            .map(|(plan, _)| &plan.checkpoints[checkpoint - 1]);
        let strategy = serde_json::to_value(self.options.context_strategy)?;
        let arm = strategy.as_str().unwrap();
        let mismatch = planned.is_some_and(|planned| {
            checkpoint_mismatch(planned, arm, &self.probes, &self.history, frontier_id)
        });
        if let Some(planned) = planned {
            let actual_ids: Vec<_> = planned
                .common_probe_ids
                .iter()
                .filter(|id| {
                    crate::probes::eligible(
                        self.probes.iter().find(|probe| &probe.id == *id).unwrap(),
                        checkpoint,
                        turn.unwrap(),
                        &self.history,
                        frontier_id,
                    )
                })
                .collect();
            let covariates = json!({"planned_arm_frontier":planned.arm_frontiers[arm],"proposed_common_probe_ids":planned.common_probe_ids,"actual_eligible_common_probe_ids":actual_ids,"actual_frontier_message_id":frontier_id,"actual_compaction_count":self.compactions.len()});
            self.turn_frontiers.last_mut().unwrap()["comparison_plan"] = covariates.clone();
            if mismatch {
                self.plan_mismatches.push(json!({"checkpoint":checkpoint,"turn":turn,"reason":"plan_frontier_or_eligibility_mismatch","covariates":covariates}));
            }
        }
        Ok(mismatch)
    }

    async fn ask_probes(
        &mut self,
        turn: Option<usize>,
        checkpoint: usize,
        frontier: Option<i64>,
        frontier_id: Option<&str>,
        mismatch: bool,
        request: &ModelRequest,
    ) -> Result<Vec<crate::probes::ProbeAnswer>> {
        let mut asked = 0;
        let mut delivered_workers = Vec::new();
        for probe in &self.probes {
            if !crate::probes::at_position(probe, turn.unwrap()) {
                continue;
            }
            let mut row = self.probe_row(probe, turn, checkpoint, frontier, frontier_id)?;
            if let Some(reason) =
                self.probe_skip_reason(probe, turn, checkpoint, frontier_id, mismatch, asked)
            {
                row["status"] = json!("skipped");
                row["skip_reason"] = json!(reason);
                self.probe_rows.push(row);
                continue;
            }
            asked += 1;
            let answer = self.answer_probe(probe, &mut row, request).await?;
            self.probe_rows.push(row);
            delivered_workers.push(answer);
        }
        Ok(delivered_workers)
    }

    fn probe_row(
        &self,
        probe: &Probe,
        turn: Option<usize>,
        checkpoint: usize,
        frontier: Option<i64>,
        frontier_id: Option<&str>,
    ) -> Result<Value> {
        let mut row = json!({"id":probe.id,"type":probe.kind,"question":probe.question,"expected_answer":probe.expected_answer,"checkpoint":checkpoint,"turn":turn,"end_step":self.history.len(),"end_message_id":self.history.last().map(|message|&message.id),"compaction_count":self.compactions.len(),"frontier":frontier,"frontier_message_id":frontier_id,"evidence_message_ids":probe.evidence_message_ids,"earliest_checkpoint":probe.earliest_checkpoint});
        let metadata = serde_json::to_value(probe)?;
        for key in [
            "ask_at_turn",
            "origin",
            "original_question",
            "source_message_id",
            "source_turn",
        ] {
            if let Some(value) = metadata.get(key) {
                row[key] = value.clone();
            }
        }
        Ok(row)
    }

    fn probe_skip_reason(
        &self,
        probe: &Probe,
        turn: Option<usize>,
        checkpoint: usize,
        frontier_id: Option<&str>,
        mismatch: bool,
        asked: usize,
    ) -> Option<&'static str> {
        let planned = self
            .pinned_plan
            .as_ref()
            .map(|(plan, _)| &plan.checkpoints[checkpoint - 1]);
        if planned.is_some_and(|planned| !planned.common_probe_ids.contains(&probe.id)) {
            Some("excluded_by_common_plan")
        } else if mismatch {
            Some("plan_frontier_mismatch")
        } else if !crate::probes::eligible(
            probe,
            checkpoint,
            turn.unwrap(),
            &self.history,
            frontier_id,
        ) {
            Some(if checkpoint < probe.earliest_checkpoint {
                "before_earliest_checkpoint"
            } else {
                "evidence_not_compacted"
            })
        } else if self.options.probe_limit.is_some_and(|limit| asked >= limit) {
            Some("probe_limit")
        } else if self.telemetry.lock().unwrap().guard().is_err() {
            Some("spend_guard")
        } else {
            None
        }
    }

    /// Ask one probe and judge its answer; the paid calls never enter the source chat.
    async fn answer_probe(
        &self,
        probe: &Probe,
        row: &mut Value,
        request: &ModelRequest,
    ) -> Result<crate::probes::ProbeAnswer> {
        let call_start = self.telemetry.lock().unwrap().calls.len();
        let answer = crate::probes::answer_with_policy(
            self.probe_model.clone(),
            &self.store,
            &self.session_id,
            request,
            probe,
            self.options.agents.clone(),
        )
        .await;
        for (key, value) in answer.as_object().unwrap() {
            row[key] = value.clone();
        }
        let call_end = self.telemetry.lock().unwrap().calls.len();
        row["probe_usage"] =
            call_totals(&self.telemetry.lock().unwrap().calls[call_start..call_end]);
        if let Some(text) = row["answer"].as_str() {
            // Judges use a stateless prompt without a worker session.
            // Their paid request belongs to the source replay chat.
            let (judgment, records) = bubblepaw_core::capture_request_usage_with_store(
                &self.store,
                &self.session_id,
                None,
                bubblepaw_core::RequestKind::Main,
                crate::probes::judge_for_session(
                    self.probe_model.as_ref(),
                    probe,
                    text,
                    &self.session_id,
                ),
            )
            .await;
            for record in records {
                self.store.queue_request_usage(record)?;
            }
            self.store.flush_request_usage(&self.session_id)?;
            row["judge"] = judgment;
            row["judge_usage"] = call_totals(&self.telemetry.lock().unwrap().calls[call_end..]);
        }
        Ok(answer)
    }
}
