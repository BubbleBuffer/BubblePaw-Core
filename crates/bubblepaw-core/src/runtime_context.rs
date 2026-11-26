//! The stock request context: the run loop's questions about the request,
//! answered over the host's preparation hook, the store and the context state.
use super::*;
use crate::request_seam::{Built, Draft, Note, Release, RequestContext, TooLarge};
use crate::{ContextState, ContextWriter};

#[derive(Default)]
pub struct FullHistory;
impl ContextPreparation for FullHistory {}

/// A session's parsed canonical history and the sizes of its messages.
#[derive(Default)]
pub(super) struct ParsedHistory {
    pub(super) frontier: Option<i64>,
    pub(super) prefix_id: Option<String>,
    pub(super) through: i64,
    pub(super) messages: Vec<Message>,
    pub(super) sizes: HashMap<String, (Message, usize)>,
}
impl ParsedHistory {
    pub(super) fn refresh(&mut self, store: &Store, session_id: &str) -> Result<()> {
        for (seq, message) in store.messages_after(session_id, self.through)? {
            self.through = seq;
            self.messages.push(message.without_details());
        }
        Ok(())
    }
    pub(super) fn request_bytes(&mut self, request: &mut ModelRequest) -> Result<usize> {
        // Serializing a message-free shell preserves the exact serde wire size,
        // including optional fields. Canonical-only chat journals are excluded
        // from message payload accounting. Existing messages are only compared and
        // counted; transformed/new messages are serialized once.
        let messages = std::mem::take(&mut request.messages);
        let shell = serde_json::to_vec(request).map(|bytes| bytes.len());
        request.messages = messages;
        let mut bytes = shell?;
        for message in &request.messages {
            let entry = self
                .sizes
                .entry(message.id.clone())
                .or_insert_with(|| (message.clone(), 0));
            if entry.1 == 0 || !runtime_admission::same_message_wire_size(&entry.0, message) {
                entry.1 = crate::prepared_message_bytes(message)?;
                entry.0 = message.clone();
            }
            bytes = bytes.saturating_add(entry.1);
        }
        Ok(bytes.saturating_add(request.messages.len().saturating_sub(1)))
    }
}

/// The one implementation: the root lineage of the store's context state.
pub(super) struct StockRequestContext {
    pub(super) hook: Arc<dyn ContextPreparation>,
    pub(super) store: Store,
    pub(super) state: Arc<RuntimeState>,
    pub(super) selection: runtime_mode::ToolSelection,
    pub(super) observation: Option<Observation>,
    pub(super) max_request_bytes: usize,
}

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
}

impl StockRequestContext {
    fn context(&self) -> &ContextState {
        &self.state.cache.context
    }

    /// The store half of publishing the applied catalog, then its cold note.
    fn update_applied_catalog(&self, session: &str) -> Result<()> {
        let tools = self.selection.selected(session, &RunOptions::default())?;
        runtime_catalog::refresh_cold_catalog(&self.store, session, &tools)?;
        let writer = ContextWriter::new(self.context(), session);
        self.stage_chat_catalog(session, &writer)
    }

    /// The catalog is a cold note like any other: it is staged through the
    /// session's ContextWriter, after the host hook's own notes.
    fn stage_chat_catalog(&self, session: &str, writer: &ContextWriter<'_>) -> Result<()> {
        if let Some(text) = runtime_catalog::cold_catalog_note(&self.store, session)? {
            writer.cold_note("tool_catalog", &text);
        }
        Ok(())
    }

    /// Run the host hook with a writer for the root lineage. Cold notes are
    /// staged afresh for each preparation; the frozen tier is noted on both
    /// sides of an ordinary request's hook so a hook-made change is reported,
    /// not declared. Manual compaction is not a main request and is not noted.
    async fn prepare_with_writer(
        &self,
        session: &str,
        request: ModelRequest,
        provider: Arc<dyn Provider>,
        mode: PrepareMode<'_>,
        cancellation: CancellationToken,
    ) -> Result<PreparedContext> {
        let context = self.context();
        context.clear_cold_notes(session);
        let mut before = ModelRequest {
            system_prompt: request.system_prompt.clone(),
            tools: request.tools.clone(),
            disabled_tools: request.disabled_tools.clone(),
            ..Default::default()
        };
        let writer = ContextWriter::new(context, session);
        let prepared = self
            .hook
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
        } else if let Some(reason) = self.hook.take_prefix_change(session) {
            self.declare_change(session, &reason);
            self.update_applied_catalog(session)?;
        }
        self.stage_chat_catalog(session, &writer)?;
        Ok(prepared)
    }

    fn build_request(
        &self,
        request: ModelRequest,
        session: &str,
        scope: Option<&ObservationScope>,
        provider: Arc<dyn Provider>,
    ) -> Result<ModelRequest> {
        let context = self.context();
        if let Some(reason) = self.hook.take_prefix_change(session) {
            self.declare_change(session, &reason);
            self.update_applied_catalog(session)?;
        }
        let cold = self.hook.cold_message_indexes(session, &request)?;
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
                        <= self.max_request_bytes,
                    "Prepared request exceeds configured {} byte limit",
                    self.max_request_bytes
                );
                self.hook
                    .validate_prepared_request(session, request, provider)?;
                Ok(())
            })?;
        if let (Some(observer), Some(scope)) = (&self.observation, scope) {
            observer.context_built(scope.clone(), &built.record);
        }
        let mut request = built.request;
        request.cache_tiers = Some(built.tiers);
        Ok(request)
    }
}

#[async_trait]
impl RequestContext for StockRequestContext {
    fn history(&self, session_id: &str) -> Result<Vec<Message>> {
        let host_frontier = self.hook.history_frontier(session_id)?;
        let (frontier, prefix) = if host_frontier.is_some() {
            (host_frontier, Vec::new())
        } else {
            self.store.imported_history_seed(session_id)?
        };
        let mut histories = self.state.histories.lock().unwrap();
        let history = histories.entry(session_id.into()).or_default();
        let prefix_id = prefix.first().map(|message| message.id.clone());
        if history.frontier != frontier || history.prefix_id != prefix_id {
            *history = ParsedHistory {
                through: frontier.unwrap_or(0),
                frontier,
                prefix_id,
                messages: prefix,
                ..Default::default()
            };
        }
        history.refresh(&self.store, session_id)?;
        Ok(history.messages.clone())
    }

    /// An incomplete reply with no user message after it (a resumed run) is left
    /// out of the projection; it stays in the store.
    fn draft(&self, session: &str, history: &[Message], steering: &[Message]) -> Result<Draft> {
        let mut messages = self.hook.project_history(history)?;
        if messages
            .last()
            .is_some_and(|last| last.incomplete.is_some())
        {
            messages.pop();
        }
        // The new batch must reach token/window admission intact,
        // even if the ordinary history projection drops older input.
        messages.retain(|saved| !steering.iter().any(|steer| steer.id == saved.id));
        messages.extend(steering.iter().cloned());
        let preview = self.reminder_preview(session)?;
        if let Some(message) = &preview {
            messages.push(message.clone());
        }
        Ok(Draft { messages, preview })
    }

    fn reminder_preview(&self, session: &str) -> Result<Option<Message>> {
        self.context()
            .preview_reminder_message(session, "root", &self.store)
    }

    async fn prepare(
        &self,
        session: &str,
        request: ModelRequest,
        provider: Arc<dyn Provider>,
        mode: PrepareMode<'_>,
        cancellation: CancellationToken,
    ) -> Result<PreparedContext> {
        self.prepare_with_writer(session, request, provider, mode, cancellation)
            .await
    }

    fn build(
        &self,
        session: &str,
        request: ModelRequest,
        scope: Option<&ObservationScope>,
        provider: Arc<dyn Provider>,
    ) -> Result<Built> {
        match self.build_request(request, session, scope, provider) {
            Ok(request) => Ok(Built::Ready(Box::new(request))),
            Err(error) => {
                let Some(budget) = error.downcast_ref::<ContextBudgetExceeded>() else {
                    return Err(error);
                };
                let (additional_tokens, additional_bytes) =
                    (budget.additional_tokens, budget.additional_bytes);
                let frontier = self.hook.history_frontier(session)?;
                Ok(Built::TooLarge(TooLarge {
                    error,
                    additional_tokens,
                    additional_bytes,
                    frontier,
                }))
            }
        }
    }

    fn note(&self, session: &str, note: Note<'_>) {
        let context = self.context();
        match note {
            Note::Text { kind, text } => context.remind(session, "root", kind, text),
            Note::Fresh { kind, text } => context.remind_fresh(session, "root", kind, text),
            Note::Reminder(reminder) => context.remind_with_metadata(session, "root", reminder),
            Note::Cold { kind, text } => context.cold_note(session, kind, text),
        }
    }

    fn declare_change(&self, session: &str, reason: &str) {
        self.state.cache.declare(Some(session), reason);
    }

    fn take_prefix_change(&self, session: &str) -> Option<String> {
        let reason = self.hook.take_prefix_change(session)?;
        self.declare_change(session, &reason);
        Some(reason)
    }

    fn tool_generation_observed(&self, session: &str, generation: u64) -> bool {
        self.context().observe_tool_generation(session, generation)
    }

    fn run_hold(&self, run: &Run) -> Box<dyn std::any::Any + Send> {
        Box::new(crate::request_metering::RunHold {
            state: self.state.clone(),
            run_id: run.id.clone(),
        })
    }

    fn turn_scope(&self, run: &Run) -> Box<dyn std::any::Any + Send> {
        Box::new(crate::quota::TurnScope::enter(&run.session_id, &run.id))
    }

    fn requester(&self, run: &Run, provider: Arc<dyn Provider>) -> Arc<dyn Provider> {
        Arc::new(crate::request_metering::Metered {
            inner: provider,
            store: self.store.clone(),
            state: self.state.clone(),
            session_id: run.session_id.clone(),
            run_id: run.id.clone(),
        })
    }

    /// Trace a request's start against the host's ledger. The wire shape is only computed when traced.
    fn model_started(
        &self,
        run: &Run,
        scope: Option<&ObservationScope>,
        request: &ModelRequest,
        provider: &Arc<dyn Provider>,
    ) {
        self.state.usage.expect(
            &run.id,
            crate::request_usage::request_fallback(&request.model, run, scope),
        );
        if let (Some(observer), Some(scope)) = (&self.observation, scope) {
            observer.model_start(
                scope.clone(),
                &self.state.cache,
                request,
                provider.wire_shape(request),
            );
        }
    }

    fn request_sent(&self, session: &str, request: &ModelRequest, provider: Arc<dyn Provider>) {
        self.hook.observe_request(session, request, provider);
    }

    /// Trace the result, then close the root lineage's request-log row with
    /// the answering attempt's usage and report an unexplained cache miss.
    fn model_finished(
        &self,
        run: &Run,
        scope: Option<&ObservationScope>,
        result: &mut Result<crate::ModelResponse>,
        (elapsed_ms, first_text_ms): (u64, Option<u64>),
        cancelled: bool,
    ) {
        let session = run.session_id.as_str();
        let record = self.state.usage.ended(&run.id, result);
        if let (Some(observer), Some(scope)) = (&self.observation, scope) {
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
        let Some(record) = record else {
            return;
        };
        let breakpoints = result
            .as_mut()
            .ok()
            .and_then(crate::RequestLogRow::take_breakpoints);
        let answered = (!record.failed).then_some(&record.usage);
        let warning = self.context().finish_request(
            session,
            "root",
            &record.provider,
            &record.model,
            answered,
            breakpoints,
        );
        if let (Some(warning), Some(observer), Some(scope)) = (warning, &self.observation, scope) {
            observer.emit(scope.clone(), "cache_miss_unexplained", warning, None);
        }
    }

    fn response_received(&self, session: &str, response: &ModelResponse) {
        self.hook.observe_response(session, response);
    }

    fn release(&self, session: &str, how: Release) {
        match how {
            Release::Deleted => {
                self.state.histories.lock().unwrap().remove(session);
                self.context().release_session(session);
            }
            Release::Idle => self.state.release_history(session),
        }
        self.hook.release_session(session);
    }
}
