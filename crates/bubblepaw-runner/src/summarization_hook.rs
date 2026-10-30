//! The preparation hook of `SummarizingContext`: the runtime inventory and cold
//! notes it writes through the writer, the `ContextPreparation` impl, and the
//! test helpers that run one preparation.
use super::*;

impl SummarizingContext {
    /// One ordinary preparation through the hook on a fresh root-lineage
    /// writer, with this context's own provider.
    #[cfg(feature = "bench")]
    pub(crate) async fn prepare_fresh(
        &self,
        session: &str,
        request: ModelRequest,
    ) -> Result<PreparedContext> {
        self.prepare_on(session, request, &bubblepaw_core::ContextState::default())
            .await
    }
    /// One replay step's request, built through `state` as the live root path
    /// builds it: a compaction runs the hook, which stages the view's notes; any
    /// other step applies the saved view and stages the notes itself. Either way
    /// the notes are placed and the request classified on `state`.
    #[cfg(feature = "bench")]
    pub(crate) async fn prepare_step(
        &self,
        session: &str,
        request: ModelRequest,
        compact: bool,
        state: &bubblepaw_core::ContextState,
    ) -> Result<PreparedContext> {
        state.clear_cold_notes(session);
        let mut prepared = if compact {
            self.prepare_on(session, request, state).await?
        } else {
            let request = self.frozen_request(session, request)?;
            self.stage_cold_notes(session, &ContextWriter::new(state, session))?;
            PreparedContext {
                request,
                notice: None,
                usage: None,
            }
        };
        prepared.request = self.build_request(session, prepared.request, state)?;
        Ok(prepared)
    }
    /// One ordinary preparation through the hook on the root-lineage writer of
    /// `state`, so the caller can place and classify the result on that state.
    #[cfg(feature = "bench")]
    pub(crate) async fn prepare_on(
        &self,
        session: &str,
        request: ModelRequest,
        state: &bubblepaw_core::ContextState,
    ) -> Result<PreparedContext> {
        let writer = ContextWriter::new(state, session);
        self.prepare(PrepareCall {
            session_id: session,
            request,
            provider: self.provider.clone(),
            writer: &writer,
            mode: PrepareMode::Request,
            cancellation: CancellationToken::new(),
        })
        .await
    }
    /// A non-compaction request built as the live root path builds it, on a fresh state.
    #[cfg(all(test, feature = "bench"))]
    pub(crate) async fn built_frozen_request(
        &self,
        session: &str,
        request: ModelRequest,
    ) -> Result<ModelRequest> {
        let state = bubblepaw_core::ContextState::default();
        Ok(self
            .prepare_step(session, request, false, &state)
            .await?
            .request)
    }
    /// The runtime inventory and the cold notes, written through the writer.
    /// Explicit compaction states the full snapshot. An ordinary request states
    /// the full one after an applied `lcm_apply` (peeked here: the harness takes
    /// the reason when it builds the request) and otherwise only the live facts,
    /// and only when automatic work or an applied view makes them relevant.
    fn write_context_notes(
        &self,
        session: &str,
        writer: &ContextWriter<'_>,
        manual: bool,
    ) -> Result<()> {
        let view = self.store.context_view(session)?;
        if let Some(notes) = &self.notes {
            if manual {
                writer.remind("runtime_inventory", &notes.note(session));
            } else if self
                .prefix_changes
                .lock()
                .unwrap()
                .get(session)
                .is_some_and(|reason| reason == "lcm_apply")
            {
                writer.remind_fresh("runtime_inventory", &notes.note(session));
            } else if self.automatic() || view.is_some() {
                writer.remind("runtime_inventory", &notes.live_note(session));
            }
        }
        self.stage_cold_notes(session, writer)
    }
}

#[async_trait]
impl ContextPreparation for SummarizingContext {
    fn validate_prepared_request(
        &self,
        session: &str,
        request: &ModelRequest,
        provider: Arc<dyn Provider>,
    ) -> Result<()> {
        self.validate_final_request(session, request, provider)
    }
    fn history_frontier(&self, session: &str) -> Result<Option<i64>> {
        Ok(self
            .store
            .context_view(session)?
            .and_then(|view| view.frontier))
    }
    async fn prepare(&self, call: PrepareCall<'_>) -> Result<PreparedContext> {
        let PrepareCall {
            session_id: session,
            request,
            provider,
            writer,
            mode,
            cancellation,
        } = call;
        let bound = Self {
            provider,
            ..self.clone()
        };
        let (manual, focus) = match mode {
            PrepareMode::Request | PrepareMode::Pressure { .. } => (false, None),
            PrepareMode::Compact { focus } => (true, focus),
        };
        let pressure = match mode {
            PrepareMode::Pressure {
                additional_tokens,
                additional_bytes,
            } => Some((additional_tokens, additional_bytes)),
            _ => None,
        };
        let prepared = bound
            .prepare_context(session, request, cancellation, manual, focus, pressure)
            .await?;
        if bound.surface_rebinds.lock().unwrap().remove(session) {
            writer.discard_pending_reminders(&["memory_files:", "skills:"]);
        }
        // Written only after preparation succeeded: it may have applied a view,
        // which decides what the inventory and cold notes state.
        bound.write_context_notes(session, writer, manual)?;
        Ok(prepared)
    }
    fn observe_request(&self, session: &str, request: &ModelRequest, provider: Arc<dyn Provider>) {
        // Response calibration must start at the input actually dispatched,
        // including the cold catalog and final combined reminder. Otherwise
        // those tokens become a false residual on the next preparation.
        let bound = Self {
            provider: provider.clone(),
            ..self.clone()
        };
        let recorded = (|| -> Result<()> {
            let checkpoint =
                if self.store.session(session)?.context_strategy == ContextStrategy::Lcm {
                    self.store.context_checkpoint(session)?
                } else {
                    None
                };
            bound.register_request(
                session,
                request,
                checkpoint.as_ref(),
                &self.store.context_chapters(session)?,
            )?;
            Ok(())
        })();
        if let Err(error) = recorded {
            tracing::warn!(%error, "Could not register final context input for calibration");
        }
        self.capture_episode_request(session, request, provider, true);
    }
    fn release_session(&self, session: &str) {
        self.release_episode_capture(session);
    }
    fn take_prefix_change(&self, session_id: &str) -> Option<String> {
        self.prefix_changes.lock().unwrap().remove(session_id)
    }
    fn cold_message_indexes(&self, session: &str, request: &ModelRequest) -> Result<Vec<usize>> {
        self.saved_view_cold_indexes(session, request)
    }
    fn observe_response(&self, session: &str, response: &ModelResponse) {
        self.observe_context_response(session, response);
    }
}

/// One ordinary preparation on a fresh root-lineage writer. The writer's state
/// is returned so a test can see what the preparation staged.
#[cfg(test)]
pub(super) async fn prepare_main(
    context: &dyn ContextPreparation,
    provider: Arc<dyn Provider>,
    session: &str,
    request: ModelRequest,
) -> Result<(bubblepaw_core::ContextState, PreparedContext)> {
    let state = bubblepaw_core::ContextState::default();
    let prepared = {
        let writer = ContextWriter::new(&state, session);
        context
            .prepare(PrepareCall {
                session_id: session,
                request,
                provider,
                writer: &writer,
                mode: PrepareMode::Request,
                cancellation: CancellationToken::new(),
            })
            .await?
    };
    Ok((state, prepared))
}

/// The request the harness builds after one ordinary preparation, so the tail
/// reminders and cold notes that preparation wrote are in it.
#[cfg(test)]
pub(super) async fn built_after_prepare(
    context: &dyn ContextPreparation,
    provider: Arc<dyn Provider>,
    store: &Store,
    session: &str,
) -> Result<ModelRequest> {
    let request = ModelRequest {
        messages: store.messages(session)?,
        ..Default::default()
    };
    let (state, prepared) = prepare_main(context, provider, session, request).await?;
    let cold = context.cold_message_indexes(session, &prepared.request)?;
    Ok(state
        .build_persisted(prepared.request, session, "root", &cold, store, |_| Ok(()))?
        .request)
}

#[cfg(test)]
pub(super) fn has_inventory(request: &ModelRequest) -> bool {
    request
        .messages
        .iter()
        .any(|message| message.content.contains("kind=\"runtime_inventory\""))
}
