//! Context tier provenance and response token calibration.
use super::*;

impl SummarizingContext {
    pub(super) fn config_for_request(
        &self,
        session: &str,
        request: &ModelRequest,
    ) -> SummaryConfig {
        let mut config = self.config_for(session, &request.model);
        config.lcm.output_reserve_tokens = config
            .lcm
            .output_reserve_tokens
            .max(request.max_output_tokens.unwrap_or(0) as usize);
        if let Some(window) = self.provider.capabilities(&request.model).context_window {
            config.context_window =
                Some(config.context_window.map_or(window, |old| old.min(window)));
        }
        config
    }
    pub(super) fn request_meter_signature(
        &self,
        session: &str,
        request: &ModelRequest,
        checkpoint: Option<&ContextCheckpoint>,
        chapters: &[ContextChapter],
    ) -> Result<String> {
        let frozen = self.store.context_view(session)?;
        Ok(bubblepaw_core::fingerprint(json!({"view":frozen,"model":request.model,"system":request.system_prompt,"tools":request.tools,"checkpoint":checkpoint.map(|c| &c.id),"chapters":chapters.iter().map(|chapter| &chapter.id).collect::<Vec<_>>(),"chapter_view":request.messages.iter().filter(|message| message.role == Role::System && message.content.starts_with("Context chapters (")).map(|message| &message.content).collect::<Vec<_>>()}).to_string().as_bytes()))
    }
    /// Read-only final admission: never prepares again, gathers, mutates a meter,
    /// or publishes a view after the final combined reminder has been appended.
    pub(super) fn validate_final_request(
        &self,
        session: &str,
        request: &ModelRequest,
        provider: Arc<dyn Provider>,
    ) -> Result<()> {
        // Ordinary no-auto history already bypasses summary admission, including
        // when a previously applied view exists. The core byte guard still runs.
        if !self.automatic {
            return Ok(());
        }
        let bound = Self {
            provider,
            ..self.clone()
        };
        let config = bound.config_for_request(session, request);
        let threshold = config.threshold();
        ensure!(
            threshold > 0,
            "Model context window must exceed the configured output reserve"
        );
        let (unadjusted, rendered) = bound.estimated_input(request);
        let checkpoint = if self.store.session(session)?.context_strategy == ContextStrategy::Lcm {
            self.store.context_checkpoint(session)?
        } else {
            None
        };
        let chapters = self.store.context_chapters(session)?;
        let signature =
            self.request_meter_signature(session, request, checkpoint.as_ref(), &chapters)?;
        let estimate = self
            .meters
            .lock()
            .unwrap()
            .get(session)
            .filter(|meter| meter.signature == signature && meter.rendered_input == rendered)
            .and_then(|meter| meter.anchor)
            .map(|(previous, measured)| {
                measured
                    .saturating_add(unadjusted)
                    .saturating_sub(previous)
                    .max(unadjusted)
            })
            .unwrap_or(unadjusted);
        let bytes = bubblepaw_core::prepared_request_bytes(request)?;
        if estimate > threshold || bytes > config.max_request_bytes {
            let meters = self.meters.lock().unwrap();
            let meter = meters.get(session);
            return Err(bubblepaw_core::ContextBudgetExceeded {
                additional_tokens: meter.map_or(0, |m| unadjusted.saturating_sub(m.pending)),
                additional_bytes: meter.map_or(0, |m| bytes.saturating_sub(m.pending_bytes)),
                message: if estimate > threshold {
                    format!("Prepared request including harness reminders exceeds the context budget ({estimate} > {threshold} tokens)")
                } else {
                    "Prepared request including harness reminders exceeds the byte budget".into()
                },
            }.into());
        }
        Ok(())
    }

    /// Match the saved view's actual prefix records in the final prepared input.
    /// Runtime controls may be inserted between them; membership is never guessed
    /// from marker text, roles or synthetic ids.
    pub(super) fn saved_view_cold_indexes(
        &self,
        session: &str,
        request: &ModelRequest,
    ) -> Result<Vec<usize>> {
        let Some(view) = self.store.context_view(session)? else {
            return Ok(Vec::new());
        };
        let mut prefix = view.prefix.iter().peekable();
        let mut indexes = Vec::new();
        for (index, message) in request.messages.iter().enumerate() {
            if prefix.peek().is_some_and(|expected| *expected == message) {
                indexes.push(index);
                prefix.next();
            }
        }
        Ok(indexes)
    }
    pub(super) fn observe_context_response(&self, session: &str, response: &ModelResponse) {
        self.observe_episode_route(session, response);
        let multiple = response.provider_state.as_ref().is_some_and(|state| {
            state["rounds"]
                .as_array()
                .is_some_and(|rounds| rounds.len() > 1)
        }) || response
            .last_request_usage
            .as_ref()
            .and_then(|last| last.input_tokens)
            .zip(response.usage.as_ref())
            .is_some_and(|(last, total)| total.input_tokens != last);
        let physical = if multiple {
            None
        } else {
            match &response.last_request_usage {
                Some(last) => last.input_tokens.map(|input| (input, last.output_tokens)),
                None => response
                    .usage
                    .as_ref()
                    .filter(|usage| usage.input_tokens > 0)
                    .map(|usage| (usage.input_tokens, Some(usage.output_tokens))),
            }
        };
        let message = Message {
            tool_calls: response.tool_calls.clone(),
            provider_state: response.provider_state.clone(),
            ..Self::synthetic(session, Role::Assistant, response.text.clone())
        };
        // A rendered projection can deliberately omit opaque native payloads.
        // Measured output still covers those tokens. Subtract only the readable
        // output represented in the next projection, avoiding a second count of
        // its thinking text. Withheld cuts remain absent from that projection.
        let opaque_blocks = response
            .provider_state
            .as_ref()
            .and_then(|state| state.get("content"))
            .filter(|blocks| {
                blocks.as_array().is_some_and(|blocks| {
                    blocks.iter().any(|block| {
                        (block["type"] == "thinking"
                            && block["signature"]
                                .as_str()
                                .is_some_and(|signature| !signature.is_empty()))
                            || (block["type"] == "redacted_thinking" && !block["data"].is_null())
                    })
                })
            });
        let mut meters = self.meters.lock().unwrap();
        if let Some(meter) = meters.get_mut(session) {
            // Final continuation input has a different shape from the original
            // pending request. Use the local projection instead of a false anchor.
            meter.anchor = physical.and_then(|(input, output)| {
                if meter.rendered_input && opaque_blocks.is_none() {
                    Some((meter.pending, input as usize))
                } else {
                    // Keep the measured residual for opaque native output,
                    // subtracting readable output represented on the next input.
                    output.map(|output| {
                        let represented = self.message_tokens(&message).saturating_add(
                            opaque_blocks
                                .map_or(0, |blocks| self.readable_thinking_tokens(blocks, false)),
                        );
                        (
                            meter.pending.saturating_add(represented),
                            input.saturating_add(output) as usize,
                        )
                    })
                }
            });
        }
    }
    /// Instructions that depend on the applied view, for the end of the cold tier.
    /// A legacy view carries its lookup note in its own prefix and gets none.
    pub(super) fn view_cold_notes(
        &self,
        session: &str,
        view: &ContextView,
    ) -> Result<Vec<(String, String)>> {
        if Self::legacy_view(view) {
            return Ok(Vec::new());
        }
        let lookup = match self.store.session(session)?.context_strategy {
            ContextStrategy::Lcm => HISTORY_RETRIEVAL_INSTRUCTION,
            ContextStrategy::Classical => STRATEGY_RETRIEVAL_INSTRUCTION,
        };
        Ok(vec![(HISTORY_LOOKUP_NOTE.into(), lookup.into())])
    }

    /// Views saved before the cold tier froze the system prompt and kept the
    /// lookup note (and the runtime inventory) as messages of their prefix.
    pub(super) fn legacy_view(view: &ContextView) -> bool {
        !view.system_prompt.is_empty()
            || view.prefix.iter().any(|message| {
                message.content == HISTORY_RETRIEVAL_INSTRUCTION
                    || message.content == STRATEGY_RETRIEVAL_INSTRUCTION
            })
    }

    /// Only a legacy view replaces the system prompt, so existing chats are unchanged.
    pub(super) fn serve_view_system_prompt(request: &mut ModelRequest, view: &ContextView) {
        if !view.system_prompt.is_empty() {
            request.system_prompt = view.system_prompt.clone();
        }
    }

    /// The LCM serving view of a request: the view's system prompt (legacy views
    /// only), the checkpoint (LCM only), the chapters and the active view's
    /// messages. The one place the view is applied, for the live hook, the
    /// episode trigger and replay.
    pub(crate) fn serving_view(
        &self,
        session: &str,
        mut request: ModelRequest,
        threshold: usize,
    ) -> Result<ModelRequest> {
        let checkpoint = if self.store.session(session)?.context_strategy == ContextStrategy::Lcm {
            self.store.context_checkpoint(session)?
        } else {
            None
        };
        let chapters = self.store.context_chapters(session)?;
        if let Some(view) = self.store.context_view(session)? {
            Self::serve_view_system_prompt(&mut request, &view);
        }
        request.messages = self.active_view(
            session,
            &request.messages,
            checkpoint.as_ref(),
            &chapters,
            threshold,
        )?;
        Ok(request)
    }

    /// Stage the applied view's cold notes on a root-lineage writer. Core places
    /// them into the request (`build_request`).
    pub(crate) fn stage_cold_notes(
        &self,
        session: &str,
        writer: &bubblepaw_core::ContextWriter<'_>,
    ) -> Result<()> {
        if let Some(view) = self.store.context_view(session)? {
            for (kind, text) in self.view_cold_notes(session, &view)? {
                writer.cold_note(&kind, &text);
            }
        }
        Ok(())
    }

    /// Place the staged cold notes after the view's cold tier and classify the
    /// request through `state`, as the live root path does.
    #[cfg(feature = "bench")]
    pub(crate) fn build_request(
        &self,
        session: &str,
        mut request: ModelRequest,
        state: &bubblepaw_core::ContextState,
    ) -> Result<ModelRequest> {
        let cold = self.saved_view_cold_indexes(session, &request)?;
        let cold = state.place_cold_notes(&mut request, session, "root", &cold);
        Ok(state.build(request, session, "root", &cold).request)
    }
}

/// The cold note kind of the hint about the history tools.
const HISTORY_LOOKUP_NOTE: &str = "history_lookup";
