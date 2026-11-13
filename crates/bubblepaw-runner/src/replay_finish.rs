//! Final replay report: boundary and chapter analysis, totals and the report files.
use super::*;

/// Chapter, decision and boundary rows derived from the persisted episodes.
struct Analysis {
    episodes: Vec<bubblepaw_core::ContextEpisode>,
    chapter_rows: Vec<Value>,
    decision_rows: Vec<Value>,
    boundary_scores: Value,
    related: Value,
}

fn message_positions(history: &[Message]) -> HashMap<&str, usize> {
    history
        .iter()
        .enumerate()
        .map(|(i, m)| (m.id.as_str(), i))
        .collect()
}

impl ReplayRun {
    pub(super) async fn finish(
        mut self,
        corpus: &ReplayCorpus,
        #[cfg(test)] prepared_trace: PreparedTrace<'_>,
    ) -> Result<Value> {
        self.imports
            .flush_profiled(&mut self.importer, &self.progress, self.compactions.len())?;
        let final_view = if self.stopped.is_none() && self.pending_calls.is_empty() {
            self.final_view(
                corpus,
                #[cfg(test)]
                prepared_trace,
            )
            .await?
        } else {
            json!({"status":"unavailable","reason":"Replay stopped or final recorded tool round is incomplete; compaction snapshots remain available"})
        };
        if let Some(task) = self.relay_task.take() {
            task.abort();
        }
        let analysis = self.analyze(corpus)?;
        self.progress
            .write("loop_finished", self.history.len(), self.compactions.len())?;
        let report = self.build_report(corpus, analysis, final_view)?;
        std::fs::write(
            self.options.out.join("report.json"),
            serde_json::to_vec_pretty(&report)?,
        )?;
        std::fs::write(self.options.out.join("report.md"), markdown(&report)?)?;
        Ok(report)
    }

    fn analyze(&self, corpus: &ReplayCorpus) -> Result<Analysis> {
        let episodes = self.store.context_episodes(&self.session_id)?;
        let chapters = self.store.context_subject_chapters(&self.session_id)?;
        let decisions = self.store.context_decisions(&self.session_id)?;
        let decision_rows: Vec<Value> = decisions.iter().map(|decision| {
        let mut row=serde_json::to_value(decision).unwrap();
        row["options"]=json!(decision.option_message_ids.iter().map(|id| {
            let message=self.history.iter().find(|m|&m.id==id);
            json!({"message_id":id,"user_excerpt":message.map(|m|m.content.chars().take(800).collect::<String>())})
        }).chain(std::iter::once(json!({"message_id":null,"option":"none"}))).collect::<Vec<_>>());
        row
    }).collect();
        let positions = message_positions(&self.history);
        let chapter_start: HashMap<&str, String> = chapters
            .iter()
            .filter_map(|c| {
                let episode = episodes
                    .iter()
                    .find(|e| e.ordinal == c.first_episode_ordinal)?;
                let start = *positions.get(episode.first_message_id.as_str())?;
                self.history[..=start]
                    .iter()
                    .rev()
                    .find(|m| is_typed_user(m))
                    .map(|m| (c.id.as_str(), m.id.clone()))
            })
            .collect();
        let materialized_boundaries: Vec<String> = chapters
            .iter()
            .skip(1)
            .filter_map(|chapter| {
                episodes
                    .iter()
                    .find(|episode| episode.ordinal == chapter.first_episode_ordinal)
                    .map(|episode| episode.first_message_id.clone())
            })
            .collect();
        let predicted = accepted_boundaries(&decisions, &self.history);
        let unmaterialized_boundaries: Vec<&String> = predicted
            .iter()
            .filter(|id| !materialized_boundaries.contains(id))
            .collect();
        let chapter_rows: Vec<Value> = chapters
            .iter()
            .map(|chapter| {
                let first = episodes
                    .iter()
                    .find(|e| e.ordinal == chapter.first_episode_ordinal);
                let last = episodes
                    .iter()
                    .find(|e| e.ordinal == chapter.last_episode_ordinal);
                let mut row = serde_json::to_value(chapter).unwrap();
                row["first_message_id"] = json!(first.map(|e| &e.first_message_id));
                row["last_message_id"] = json!(last.map(|e| &e.last_message_id));
                row["start_record_index"] =
                    json!(first.and_then(|e| positions.get(e.first_message_id.as_str())));
                row["end_record_index"] =
                    json!(last.and_then(|e| positions.get(e.last_message_id.as_str())));
                row["start_typed_record"] = json!(chapter_start.get(chapter.id.as_str()));
                row
            })
            .collect();
        let boundary_scores = corpus.boundary_gold.as_ref().map(|gold| {
        let starts = string_array(&gold["gold"]); let ambiguous = string_array(&gold["ambiguous"]);
        let typed: HashMap<&str,usize> = corpus.messages.iter().filter(|m| is_typed_user(m)).enumerate().map(|(i,m)| (m.id.as_str(),i)).collect();
        json!({"source":gold["source"],"predicted":predicted,"gold":starts,"accepted_boundary_count":predicted.len(),"materialized_chapter_count":chapters.len(),"materialized_boundaries":materialized_boundaries,"unmaterialized_boundaries":unmaterialized_boundaries,"exact":boundary_score(&predicted,&starts,&typed,&ambiguous,false),"tolerant":boundary_score(&predicted,&starts,&typed,&ambiguous,true),"prediction_basis":"Unique persisted accepted episode-start choices in canonical order, excluding the initial typed chat-start record. Decisions follow episode summarization and may mark a phase transition during continuing agent work.","definition":"pipeline_trial.py maximum one-to-one cardinality, typed distance <= 1 or predicted ambiguous; ambiguous records can match any gold once"})
    }).unwrap_or_else(|| json!({"status":"unavailable","reason":"No verified boundary gold exists for this chat; template labels are not gold","predicted":predicted,"accepted_boundary_count":predicted.len(),"materialized_chapter_count":chapters.len(),"unmaterialized_boundaries":unmaterialized_boundaries}));
        let mut predicted_related = Vec::new();
        for chapter in &chapters {
            for (i, target) in chapter.related_ids.iter().enumerate() {
                let evidence = chapter.related_evidence.get(i);
                predicted_related.push(json!({"source_chapter":chapter.id,"target_chapter":target,"source_start_record":chapter_start.get(chapter.id.as_str()),"target_start_record":chapter_start.get(target.as_str()),"quote":evidence.map(|e| &e.quote),"quote_matched":evidence.and_then(|e|e.related_quote_matched)}));
            }
        }
        let related = related_score(
            corpus,
            &self.history,
            &episodes,
            &chapters,
            &predicted_related,
        );
        Ok(Analysis {
            episodes,
            chapter_rows,
            decision_rows,
            boundary_scores,
            related,
        })
    }

    fn build_report(
        &self,
        corpus: &ReplayCorpus,
        analysis: Analysis,
        final_view: Value,
    ) -> Result<Value> {
        let Analysis {
            episodes,
            chapter_rows,
            decision_rows,
            boundary_scores,
            related,
        } = analysis;
        let positions = message_positions(&self.history);
        let meter = self.telemetry.lock().unwrap();
        let episode_rows: Vec<Value> = episodes.iter().map(|e| json!({"episode":e,"kind":meter.episode_kinds.get(&e.first_message_id),"start_record_index":positions.get(e.first_message_id.as_str()),"end_record_index":positions.get(e.last_message_id.as_str()),"summary_tokens":token_count(&e.summary).unwrap_or(0),"summary_chars":e.summary.chars().count()})).collect();
        let mut by_kind = BTreeMap::<String, Value>::new();
        let mut input = 0_u64;
        let mut output = 0_u64;
        let mut reasoning = 0_u64;
        let mut unknown_reasoning = 0;
        for call in &meter.calls {
            let kind = call["kind"].as_str().unwrap_or("unknown").to_string();
            let entry = by_kind
            .entry(kind)
            .or_insert(json!({"calls":0,"input_tokens":0,"output_tokens":0,"cache_read_tokens":0,"cache_creation_tokens":0,"reasoning_tokens":0}));
            entry["calls"] = json!(entry["calls"].as_u64().unwrap() + 1);
            for name in [
                "input_tokens",
                "output_tokens",
                "cache_read_tokens",
                "cache_creation_tokens",
                "reasoning_tokens",
            ] {
                entry[name] =
                    json!(entry[name].as_u64().unwrap() + call[name].as_u64().unwrap_or(0));
            }
            input += call["input_tokens"].as_u64().unwrap_or(0);
            output += call["output_tokens"].as_u64().unwrap_or(0);
            reasoning += call["reasoning_tokens"].as_u64().unwrap_or(0);
            unknown_reasoning += usize::from(call["reasoning_tokens"].is_null());
        }
        let storage = self.store.storage_stats()?;
        let import_transactions = self.imports.committed_transactions;
        let store_transactions = storage
            .transaction_count
            .saturating_sub(import_transactions);
        let mut report = json!({"chat":corpus.chat_id,"mode":if self.options.dry_run {"scripted"} else {"live"},"session_id":self.session_id,"database":"store","data_dir":"store","recorded_model":corpus.recorded_model,"context_window":corpus.context_window,"threshold":self.threshold,"status":if self.stopped.is_some() {"stopped"} else {"complete"},"stop_reason":self.stopped,"messages_replayed":self.history.len(),"messages_total":corpus.messages.len(),"episodes":episode_rows,"chapters":chapter_rows,"decisions":decision_rows,"compactions":self.compactions,"final_view":final_view,"boundary_scores":boundary_scores,"related":related,"totals":{"calls_by_kind":by_kind,"input_tokens":input,"output_tokens":output,"reasoning_tokens":reasoning,"reasoning_usage_unavailable_calls":unknown_reasoning,"flash_estimated_cost_usd":meter.flash_cost_usd,"jev_reported_cost_usd":meter.jev_cost_usd,"haiku_estimated_cost_usd":meter.haiku_cost_usd,"model_estimated_cost_usd":meter.flash_cost_usd+meter.haiku_cost_usd+meter.openai_cost_usd,"estimated_cost_usd":meter.flash_cost_usd+meter.jev_cost_usd+meter.openai_cost_usd+meter.haiku_cost_usd,"reserved_unknown_cost_usd":meter.reserved_unknown_cost_usd,"price_basis":PRICE_BASIS,"wall_seconds":self.started.elapsed().as_secs_f64(),"failures":self.failures.len(),"physical_failed_attempts":meter.failures,"logical_failures":self.failures.len(),"retries":meter.retries},"calls":meter.calls,"failures":self.failures,"storage":storage,"storage_transaction_basis":{"store_transactions":store_transactions,"canonical_import_transactions":import_transactions,"schema_initialization_excluded":true},"provenance":corpus.provenance,"limitations":["Recorded canonical history replay, no main-model continuation; optional isolated recall probes are not downstream task acceptance","Original recorded tools are never executed","Last incomplete pack and open chapter remain pending, as in production","Input estimates use production o200k estimator without live main-provider calibration","Unknown provider usage remains reserved and is not reported as measured spend"]});
        if meter.calls.iter().any(|call| {
            call["model_address"]
                .as_str()
                .is_some_and(|address| address.starts_with("openai/"))
        }) {
            report["totals"]["openai_estimated_cost_usd"] = json!(meter.openai_cost_usd);
            report["totals"]["price_basis"] = json!(PRICE_BASIS);
        }
        self.add_run_metadata(&mut report, &meter)?;
        add_call_totals(&mut report, &meter, self.options.dry_run);
        if self.options.context_strategy == ContextStrategy::Classical {
            report["limitations"].as_array_mut().unwrap().push(json!(
            "Files touched uses recorded structured read/edit/write path arguments. Opaque JavaScript source and transient nested reads do not provide a persisted file trail."
        ));
        }
        Ok(report)
    }

    fn add_run_metadata(&self, report: &mut Value, meter: &Telemetry) -> Result<()> {
        report["phase_timings"] = self.progress.timings();
        report["archive_index"] = json!({"name":"messages_id","setup_seconds":self.archive_setup_seconds,"scope":"Production schema message-ID index; migration excluded from application/import transaction counters; index maintenance included in canonical commits"});
        report["context_strategy"] = serde_json::to_value(self.options.context_strategy)?;
        report["summary_input"] = serde_json::to_value(self.episode_policy.summary_input)?;
        report["totals"]["cache_read_tokens"] = json!(
            meter
                .calls
                .iter()
                .filter_map(|call| call["cache_read_tokens"].as_u64())
                .sum::<u64>()
        );
        report["totals"]["cache_creation_tokens"] = json!(
            meter
                .calls
                .iter()
                .filter_map(|call| call["cache_creation_tokens"].as_u64())
                .sum::<u64>()
        );
        report["compaction_count"] = json!(self.compactions.len());
        report["view_tokens_per_turn"] = json!(self.view_tokens_per_turn);
        report["rolling_checkpoint"] =
            serde_json::to_value(self.store.context_checkpoint(&self.session_id)?)?;
        report["partial"] = json!(self.stopped.is_some() || !self.plan_mismatches.is_empty());
        if self.stopped.is_none() && !self.plan_mismatches.is_empty() {
            report["status"] = json!("partial");
        }
        report["max_cost_usd"] = json!(meter.max_cost_usd);
        report["corpus_sha256"] = json!(self.corpus_sha256);
        report["source_messages_total"] = json!(self.archive_normalization.source_messages_total);
        let marker_count = self
            .archive_normalization
            .markers_replayed(self.history.len());
        report["source_messages_replayed"] = json!(self.history.len() - marker_count);
        report["archive_unknown_outcome_markers"] = json!(marker_count);
        report["archive_unknown_outcome_markers_total"] =
            json!(self.archive_normalization.marker_steps.len());
        report["archive_normalization"] = self.archive_normalization.metadata();
        report["turn_frontiers"] = json!(self.turn_frontiers);
        report["probe_plan"] = json!(
            self.pinned_plan
                .as_ref()
                .map(|(plan, digest)| json!({"plan_sha256":digest,"plan":plan}))
        );
        report["probes"] = json!({"metadata":crate::probes::metadata_for(&self.probe_model.model, self.probe_model.effort),"file":self.options.probes,"probe_set_sha256":crate::probes::fingerprint(&self.probes)?,"checkpoints_at_turns":self.options.checkpoints_at_turns,"checkpoint_semantics":"End of complete typed-user turn K; ask_at_turn fixes an individual probe position; earliest_checkpoint is its ordinal in the configured position list","probe_limit_per_checkpoint":self.pinned_plan.as_ref().map(|(plan,_)|plan.probe_limit_per_checkpoint).unwrap_or(self.options.probe_limit),"rows":self.probe_rows,"groups":crate::probes::aggregate(&self.probe_rows,serde_json::to_value(self.options.context_strategy)?.as_str().unwrap()),"unreached_turn_checkpoints":self.options.checkpoints_at_turns.iter().filter(|turn| !self.turn_frontiers.iter().any(|row|row["turn"].as_u64()==Some(**turn as u64))).collect::<Vec<_>>(),"plan_mismatches":self.plan_mismatches,"scripted_grades_are_synthetic":self.options.dry_run});
        Ok(())
    }
}

fn add_call_totals(report: &mut Value, meter: &Telemetry, dry_run: bool) {
    report["totals"]["probe"] = call_totals(
        &meter
            .calls
            .iter()
            .filter(|call| call["kind"] == "probe")
            .cloned()
            .collect::<Vec<_>>(),
    );
    report["totals"]["judge"] = call_totals(
        &meter
            .calls
            .iter()
            .filter(|call| call["kind"] == "judge")
            .cloned()
            .collect::<Vec<_>>(),
    );
    report["totals"]["summarization"] = call_totals(
        &meter
            .calls
            .iter()
            .filter(|call| matches!(call["kind"].as_str(), Some("summary" | "chapter")))
            .cloned()
            .collect::<Vec<_>>(),
    );
    report["totals"]["decision"] = call_totals(
        &meter
            .calls
            .iter()
            .filter(|call| call["kind"] == "decision")
            .cloned()
            .collect::<Vec<_>>(),
    );
    report["totals"]["projected_paid_cost_usd"] = json!(
        meter
            .calls
            .iter()
            .filter_map(|call| call["projected_paid_cost_usd"]
                .as_f64()
                .or_else(|| call["cost_usd"].as_f64()))
            .sum::<f64>()
    );
    report["totals"]["projection_basis"] = json!(format!(
        "Scripted O200k estimates use legacy Chat framing, including for Haiku; synthetic output sizes and no cache discounts, no paid usage or exact provider token claim. Decisions projected as Luna input only. Live physical calls reserve the selected model worst input/cache-creation rate and configured maximum output. {}",
        PRICE_BASIS
    ));
    let summary_calls: Vec<_> = meter
        .calls
        .iter()
        .filter(|call| call["kind"] == "summary")
        .collect();
    report["totals"]["summary"] = json!({
        "calls":summary_calls.len(),
        "input_tokens":summary_calls.iter().filter_map(|call| call["input_tokens"].as_u64()).sum::<u64>(),
        "output_tokens":summary_calls.iter().filter_map(|call| call["output_tokens"].as_u64()).sum::<u64>(),
        "estimated_cost_usd":summary_calls.iter().filter_map(|call| call["cost_usd"].as_f64()).sum::<f64>(),
        "token_basis":if dry_run { "Scripted O200k request estimates and synthetic output sizes; no provider calls or measured usage" }
            else { "Provider-reported attempt usage" },
    });
}
