//! Fresh side-call dispatch and bounded recovery for a missing prose answer.
use super::*;

/// Generated controls must never become source events or typed requests.
pub(super) fn empty_summary_retry(session: &str) -> Message {
    Message {
        id: String::new(),
        session_id: session.into(),
        role: Role::User,
        origin: Some(MessageOrigin::Generated),
        content: "HOST-GENERATED SUMMARY RETRY CONTROL (not a source event or human request). Write the requested summary now; your previous reply contained no summary text.".into(),
        tool_calls: Vec::new(),
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: String::new(),
    }
}

impl HttpEpisodeModels {
    pub(super) async fn chat(
        &self,
        kind: &str,
        session: &str,
        system: String,
        user: String,
        reason: Option<&str>,
    ) -> Result<ChatResult> {
        let (roles, registry) = self.current_models();
        let role: &ModelRole = if kind == "chapter" {
            &roles.chapter
        } else {
            &roles.summary
        };
        let mut total = Usage::default();
        let mut last_error = None;
        for candidate in &role.models {
            if !registry.llm_available(candidate) {
                continue;
            }
            'output_attempts: for output_attempt in 1..=role.repeats.unwrap_or(1) {
                let model = registry.route(candidate).2;
                let request = ModelRequest {
                    model: candidate.clone(),
                    reasoning_effort: Some(role.reasoning_effort),
                    output_schema: None,
                    system_prompt: system.clone(),
                    messages: vec![Message {
                        id: String::new(),
                        session_id: session.into(),
                        role: Role::User,
                        origin: None,
                        content: user.clone(),
                        tool_calls: Vec::new(),
                        tool_call_id: None,
                        provider_state: None,
                        is_error: false,
                        created_at: String::new(),
                    }],
                    tools: Vec::new(),
                    disabled_tools: Vec::new(),
                    max_output_tokens: role.max_output_tokens,
                    // Stateless episode requests must not enter a stateful main
                    // provider's session mirror; the chat header uses message session.
                    session_id: String::new(),
                    ..Default::default()
                };
                let mut request = request;
                for empty_attempt in 0..2 {
                    let started = Instant::now();
                    let sent =
                        self.context_route
                            .build_side(request.clone(), session, kind, reason);
                    // This existing provider override applies the role's reasoning. It does
                    // not impose JSON formatting; classical still uses the summary
                    // role and a tools-disabled ordinary text request. Keep LCM calls
                    // on the original generation path.
                    let response = registry
                        .generate_auxiliary(
                            sent,
                            CancellationToken::new(),
                            kind == "classical",
                            self.config.retry_base_delay,
                        )
                        .await;
                    if let Ok(response) = &response {
                        self.context_route
                            .finish_response(session, kind, &model, response);
                    }
                    let parsed_http = response.as_ref().err().and_then(decision_error_usage);
                    let usage = response
                        .as_ref()
                        .ok()
                        .and_then(|r| r.usage.clone())
                        .or_else(|| {
                            response
                                .as_ref()
                                .err()
                                .and_then(bubblepaw_core::ProviderError::of)
                                .and_then(|error| error.usage.clone())
                        })
                        .or_else(|| parsed_http.cloned().map(Into::into))
                        .unwrap_or_default();
                    total.add(&usage);
                    let actual = response
                        .as_ref()
                        .ok()
                        .and_then(ProviderRegistry::selected_model)
                        .and_then(|route| route.split_once('/').map(|(_, model)| model))
                        .unwrap_or(&model)
                        .to_owned();
                    match response {
                        Ok(response) => {
                            let reasoning = response
                                .provider_state
                                .as_ref()
                                .and_then(ProviderRegistry::reasoning);
                            let result = ChatResult {
                                text: response.text,
                                model: actual,
                                usage: usage.clone(),
                                reasoning,
                            };
                            let empty =
                                response.tool_calls.is_empty() && result.text.trim().is_empty();
                            let validation = if response.tool_calls.is_empty() {
                                self.validate_summary_output_at(
                                    kind,
                                    session,
                                    &result,
                                    &result.text,
                                    output_attempt,
                                )
                            } else {
                                Err(self.output_failure(
                                    kind,
                                    session,
                                    &result.model,
                                    ("tool_call_returned", None),
                                    output_attempt,
                                    usage.clone(),
                                ))
                            };
                            self.trace_call(
                                kind,
                                &result.model,
                                &usage,
                                started,
                                if validation.is_ok() { "ok" } else { "error" },
                            );
                            match validation {
                                Ok(()) => {
                                    return Ok(ChatResult {
                                        usage: total,
                                        ..result
                                    });
                                }
                                Err(error) => {
                                    last_error = Some(error);
                                    if empty && empty_attempt == 0 {
                                        request.messages.push(empty_summary_retry(session));
                                        continue;
                                    }
                                    break;
                                }
                            }
                        }
                        Err(error) => {
                            self.trace_call(kind, &actual, &usage, started, "error");
                            let stopped = bubblepaw_core::ProviderError::of(&error)
                                .is_some_and(|error| error.message == "cancelled");
                            if stopped {
                                return Err(with_episode_usage(error, total));
                            }
                            last_error = Some(error);
                            break 'output_attempts;
                        }
                    }
                }
            }
        }
        Err(with_episode_usage(
            last_error.unwrap_or_else(|| anyhow!("No available model for {kind} role")),
            total,
        ))
    }
}
