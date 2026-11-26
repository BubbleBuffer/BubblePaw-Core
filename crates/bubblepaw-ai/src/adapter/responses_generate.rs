//! The Responses adapter's `Provider` implementation and its round loop.
use super::*;

#[async_trait]
impl Provider for ResponsesProvider {
    fn models(&self) -> Vec<ModelInfo> {
        self.models
            .iter()
            .map(|id| ModelInfo {
                id: id.clone(),
                name: id.clone(),
            })
            .collect()
    }
    fn capabilities(&self, _model: &str) -> Capabilities {
        // No model window table is built in; the summarizer reads Codex metadata.
        Capabilities {
            context_window: None,
            cache: CacheMode::Prefix,
            tool_surface: ToolSurface::Direct,
            stateful: false,
            image_input: true,
        }
    }
    async fn generate_decision(
        &self,
        request: ModelRequest,
        on_text: StreamCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        self.generate_body(request, true, on_text, cancellation)
            .await
    }
    async fn generate(
        &self,
        request: ModelRequest,
        on_text: StreamCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        self.generate_body(request, false, on_text, cancellation)
            .await
    }
    fn supports_summary_fork(&self, _model: &str) -> bool {
        true
    }
    fn summary_fork_mapping(&self, request: &ModelRequest) -> Result<Option<Value>> {
        self.fork_mapping(request).map(Some)
    }
    async fn generate_summary_fork(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        tool_wire::generate_summary_fork(self, request, cancellation).await
    }
    fn estimate_input_tokens(
        &self,
        request: &ModelRequest,
        count: &dyn Fn(&Value) -> usize,
    ) -> Result<Option<usize>> {
        self.token_input(request).map(|input| Some(count(&input)))
    }
    fn wire_shape(&self, request: &ModelRequest) -> Option<crate::WireShape> {
        // `body` is pure; `generate_body` posts this body for main-loop rounds.
        let body = self.body(request).ok()?;
        let mut shape =
            crate::WireShape::from_body(&body, "input", crate::DEFAULT_CACHE_BLOCK_TOKENS);
        shape.model = request.model.clone();
        Some(shape)
    }
}
impl ResponsesProvider {
    /// Sends the reply as one or more requests. A length stop is continued; the
    /// rounds are joined into one response. `decision` keeps the request's own
    /// reasoning effort, as decision calls do.
    async fn generate_body(
        &self,
        request: ModelRequest,
        decision: bool,
        on_text: StreamCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        let (on_text, published) = track_text_publication(on_text);
        let mut known_usage = None::<Usage>;
        let mut known_quota = Vec::new();
        let result = async {
            let route_key = self.opencode_route.then(|| opencode_session(&request));
            let mut request = request;
            let mut rounds = Vec::new();
            loop {
                let mut body = self
                    .body(&request)
                    .map_err(|error| classify(ProviderErrorKind::Fatal, error))?;
                if decision {
                    body["reasoning"] =
                        json!({"effort":request.reasoning_effort.unwrap_or_default().as_str()});
                }
                let (mut response, cut) = self
                    .send_round(
                        &body,
                        &on_text,
                        &cancellation,
                        &published,
                        route_key.as_deref(),
                    )
                    .await?;
                if let Some(usage) = &response.usage {
                    known_usage.get_or_insert_with(Usage::default).add(usage);
                }
                known_quota.extend(response.quota.iter().cloned());
                if let Some(state) = response.provider_state.take() {
                    response.provider_state = Some(crate::with_provider_state_origin(
                        state,
                        "responses",
                        Some(&request.model),
                        "native",
                    )?);
                }
                let has_calls = !response.tool_calls.is_empty();
                let continuations = rounds.len();
                // The continuation carries this round, not just its visible text.
                let next = continuation::should_continue(cut, has_calls, rounds.len())
                    .then(|| continuation::continue_response(&request, &response));
                rounds.push(response);
                if let Some(next) = next {
                    request = next;
                    continue;
                }
                if cut {
                    continuation::warn_incomplete("responses", &request.model, continuations);
                }
                let mut joined = join_rounds(rounds);
                if let Some(state) = joined.provider_state.take() {
                    joined.provider_state = Some(crate::with_provider_state_origin(
                        replay_state::slim(state, &joined.text, &joined.tool_calls)?,
                        "responses",
                        Some(&request.model),
                        "native",
                    )?);
                }
                return Ok(joined);
            }
        }
        .await;
        result.map_err(|error| {
            mark_published(
                retain_prior_quota(retain_prior_usage(error, known_usage.as_ref()), known_quota),
                published.load(Ordering::Acquire),
            )
        })
    }

    /// One request. The registry owns transport retries, so this sends a single
    /// attempt and returns the reply and whether it stopped on the output limit.
    async fn send_round(
        &self,
        body: &Value,
        on_text: &StreamCallback,
        cancellation: &CancellationToken,
        published: &AtomicBool,
        route_key: Option<&str>,
    ) -> Result<(ModelResponse, bool)> {
        let key = uuid::Uuid::new_v4().to_string();
        let mut retried_auth = false;
        self.attempt(
            body,
            on_text,
            cancellation,
            &key,
            &mut retried_auth,
            route_key,
        )
        .await
        .map_err(|error| mark_published(error, published.load(Ordering::Acquire)))
    }
}
