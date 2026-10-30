//! Final request pass-through and the host preparation boundary.
use super::*;

#[derive(Default)]
pub struct FullHistory;
impl ContextPreparation for FullHistory {}

impl Harness {
    /// A message-free request using the saved chat prefix and current model.
    /// Before first admission this previews a default initial prefix without
    /// persisting it. Once bound, config and registry changes cannot alter it.
    pub fn request_template(&self, session: &str) -> Result<ModelRequest> {
        let (model, _) = self.provider_binding();
        let surface = self.chat_surface(session, &RunOptions::default(), false)?;
        Ok(ModelRequest {
            model,
            system_prompt: surface.system_prompt,
            tools: surface.declarations,
            ..Default::default()
        })
    }
    /// Run the host hook with a writer for the root lineage. Cold notes are
    /// staged afresh for each preparation; the frozen tier is noted on both
    /// sides of an ordinary request's hook so a hook-made change is reported,
    /// not declared. Manual compaction is not a main request and is not noted.
    pub(super) async fn prepare_with_writer(
        &self,
        session: &str,
        request: ModelRequest,
        provider: Arc<dyn Provider>,
        mode: PrepareMode<'_>,
        cancellation: CancellationToken,
    ) -> Result<PreparedContext> {
        let context = self.context_state();
        context.clear_cold_notes(session);
        let mut before = ModelRequest {
            system_prompt: request.system_prompt.clone(),
            tools: request.tools.clone(),
            disabled_tools: request.disabled_tools.clone(),
            ..Default::default()
        };
        let writer = crate::ContextWriter::new(context, session);
        let prepared = self
            .config
            .context
            .prepare(PrepareCall {
                session_id: session,
                request,
                provider,
                writer: &writer,
                mode,
                cancellation,
            })
            .await?;
        if !matches!(mode, PrepareMode::Compact { .. }) {
            // A committed apply may deliberately rebind host prompt content.
            // Keep arbitrary hook edits detectable, but compare against the new
            // authoritative surface when the prepared prompt exactly matches it.
            if before.system_prompt != prepared.request.system_prompt
                && let Some(surface) = self.store.chat_tool_surface(session)?
                && surface.system_prompt == prepared.request.system_prompt
            {
                before.system_prompt = surface.system_prompt;
            }
            context.note_preparation(session, "root", &before, &prepared.request);
        } else if let Some(reason) = self.config.context.take_prefix_change(session) {
            self.declare_context_change(session, &reason);
            self.update_applied_catalog(
                session,
                &self.selected_tools(session, &RunOptions::default())?,
            )?;
        }
        self.stage_chat_catalog(session, &writer)?;
        Ok(prepared)
    }
    pub(super) fn context_state(&self) -> &crate::ContextState {
        &self.state.cache.context
    }
    pub(super) fn release_context_state(&self, session: &str) {
        self.state.cache.context.release_session(session);
    }
    pub(super) fn build_context_request(
        &self,
        request: ModelRequest,
        session: &str,
        scope: Option<&ObservationScope>,
        provider: Arc<dyn Provider>,
    ) -> Result<ModelRequest> {
        let context = self.context_state();
        if let Some(reason) = self.config.context.take_prefix_change(session) {
            self.state.cache.declare(Some(session), &reason);
            self.update_applied_catalog(
                session,
                &self.selected_tools(session, &RunOptions::default())?,
            )?;
        }
        let cold = self
            .config
            .context
            .cold_message_indexes(session, &request)?;
        context.attach_request_log(&self.store);
        let built =
            context.build_persisted(request, session, "root", &cold, &self.store, |request| {
                ensure!(
                    self.state
                        .histories
                        .lock()
                        .unwrap()
                        .entry(session.into())
                        .or_default()
                        .request_bytes(request)?
                        <= self.config.max_request_bytes,
                    "Prepared request exceeds configured {} byte limit",
                    self.config.max_request_bytes
                );
                self.config
                    .context
                    .validate_prepared_request(session, request, provider)?;
                Ok(())
            })?;
        if let (Some(observer), Some(scope)) = (&self.config.observation, scope) {
            observer.context_built(scope.clone(), &built.record);
        }
        let mut request = built.request;
        request.cache_tiers = Some(built.tiers);
        Ok(request)
    }
    /// Trace a request's start against the host's ledger. The wire shape is only computed when traced.
    pub(super) fn observe_model_start(
        &self,
        scope: Option<&ObservationScope>,
        request: &ModelRequest,
        provider: &Arc<dyn Provider>,
    ) {
        if let (Some(observer), Some(scope)) = (&self.config.observation, scope) {
            observer.model_start(
                scope.clone(),
                &self.state.cache,
                request,
                provider.wire_shape(request),
            );
        }
    }
    /// Trace the result, then close the root lineage's request-log row with
    /// the answering attempt's usage and report an unexplained cache miss.
    pub(super) fn observe_model_result(
        &self,
        session: &str,
        usage: &[crate::RequestUsage],
        scope: Option<&ObservationScope>,
        result: &mut Result<crate::ModelResponse>,
        (elapsed_ms, first_text_ms): (u64, Option<u64>),
        cancelled: bool,
    ) {
        if let (Some(observer), Some(scope)) = (&self.config.observation, scope) {
            match result {
                Ok(response) => observer.model_finish(
                    scope.clone(),
                    &self.state.cache,
                    response,
                    elapsed_ms,
                    first_text_ms,
                ),
                Err(error) => {
                    if let Some(id) = &scope.model_request_id {
                        self.state.cache.abandon(id);
                    }
                    observer.failure(
                        scope.clone(),
                        "model_failed",
                        elapsed_ms,
                        if cancelled {
                            "cancelled"
                        } else {
                            "provider_error"
                        },
                        &error.to_string(),
                    )
                }
            }
        }
        let Some(record) = usage.iter().rev().find(|r| !r.failed).or(usage.last()) else {
            return;
        };
        let breakpoints = result
            .as_mut()
            .ok()
            .and_then(crate::RequestLogRow::take_breakpoints);
        let answered = (!record.failed).then_some(&record.usage);
        let warning = self.context_state().finish_request(
            session,
            "root",
            &record.provider,
            &record.model,
            answered,
            breakpoints,
        );
        if let (Some(warning), Some(observer), Some(scope)) =
            (warning, &self.config.observation, scope)
        {
            observer.emit(scope.clone(), "cache_miss_unexplained", warning, None);
        }
    }
}
