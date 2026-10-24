//! Full-prefix fork assembly and dispatch; provider decorators do not rebuild it.
use super::*;
use bubblepaw_core::{ModelResponse, Reminder, request_components};
#[path = "episode_models_admission.rs"]
mod admission;
#[path = "episode_models_delivery.rs"]
mod delivery;
pub(super) use delivery::ForkDelivery;

pub(super) async fn summarize_fork_attempt(
    models: &HttpEpisodeModels,
    input: &SummaryInput,
    fork: &EpisodeFork,
    route: &str,
    attempt: usize,
) -> Result<SummaryResult> {
    let mut request = fork.request.clone();
    request.output_schema = None;
    request.model = route.to_owned();
    let unavailable = |reason: &str| models.fork_unavailable(&input.session_id, route, reason);
    if !fork.provider.supports_summary_fork(&request.model) {
        return Err(unavailable("unsupported_protocol"));
    }
    let delivery = ForkDelivery::new(&request, "submit_summary");
    let first = request
        .messages
        .iter()
        .position(|message| message.id == fork.first_message_id);
    let last = request
        .messages
        .iter()
        .position(|message| message.id == fork.last_message_id);
    let (Some(first), Some(last)) = (first, last) else {
        return Err(unavailable("source_range_missing"));
    };
    if first > last {
        return Err(unavailable("source_range_invalid"));
    }
    // Never let a concurrent main continuation leak future events into an
    // earlier episode, even when a caller supplies a later capture.
    request.messages.truncate(last + 1);
    let fork_point = request.messages.len();
    let mut mapping = match fork.provider.summary_fork_mapping(&request) {
        Ok(Some(mapping)) => mapping,
        _ => {
            return Err(unavailable("source_mapping_unavailable"));
        }
    };
    let selected_ids: std::collections::HashSet<_> = request.messages[first..=last]
        .iter()
        .map(|message| message.id.as_str())
        .collect();
    let Some(sources) = mapping.get_mut("sources").and_then(Value::as_array_mut) else {
        return Err(unavailable("source_mapping_unavailable"));
    };
    // Earlier rows remain intact in the input. Only the episode's new
    // events need source-ID mapping in this appended range selector.
    sources.retain(|source| {
        source["source_id"]
            .as_str()
            .is_some_and(|id| selected_ids.contains(id))
    });
    let mut control = format!(
        "EPISODE MEMORY BRANCH (host-generated summary control, not a source event or human request). Write episode memory; do not continue the workspace task.\nSummarize source range {} through {}, inclusive. Only the selected mapped source rows supply provenance for events, human requests, delegated instructions, corrections and task transitions. Earlier material supplies context only.\nUse the mapping's origin and is_typed_user fields, not the wire role alone: is_typed_user=true identifies a human request; origin=child_task identifies a delegated task; origin=generated identifies host controls or notices. Delegated tasks and generated host controls are never human requests, even when their wire role is user.\nThese branch instructions, the source mapping, task anchor, previous summary, final summary-return request and any continuation controls are summarization scaffolding: they are NOT source events or human requests. Never describe this scaffolding as a human redirect, changed task, new goal or decision, and never cite it as source evidence. A previous summary or task anchor supplies context only; it cannot establish a new event in this episode.\nWrite the changes within the selected range, using previous memory only as briefly labeled prior context. Bind every cited source ID to that row's actual text, role and origin in this request. Wire or prepared positions quoted inside previous summaries are historical and may now point to different messages: ignore them and use only the current mapping. Cite stable source message and tool-call IDs in the memory; never store wire or prepared position numbers. Do not relabel an assistant acknowledgment as a typed request or repeat earlier tool evidence as a new result. END is a snapshot boundary, not an interruption: if a reply is absent by END, say \"not yet observed by END\"; do not infer that summarization interrupted, cancelled or redirected the main task.\n{}\nExact selected source-message/tool-call mapping to rendered input positions:\n{}\n\n{}",
        fork.first_message_id,
        fork.last_message_id,
        delivery.instruction(),
        mapping,
        summary_instructions(input)
    );
    if let Some(turn) = &input.turn_request {
        control.push_str(&format!(
                "\n\nTASK ANCHOR (read-only context, may be a child task; not a new source event or request):\n{turn}"
            ));
    }
    if let Some(previous) = &input.previous_summary {
        control.push_str(&format!("\n\nPREVIOUS EPISODE (read-only context; not selected source events or a new request):\n{previous}"));
    }
    for (role, content) in [
        (Role::System, control),
        (
            Role::User,
            format!(
                "HOST-GENERATED SUMMARY RETURN CONTROL (not a source event or human request). Return the selected episode summary now. Only selected mapped source rows supply event and request provenance. Do not summarize the branch instructions, mapping, task anchor, previous summary, this return control or any continuation controls as a human redirect or task event. Preserve exact useful source IDs and tool-call IDs from the mapping. {}",
                delivery.instruction()
            ),
        ),
    ] {
        request.messages.push(Message {
            id: String::new(),
            session_id: input.session_id.clone(),
            role,
            origin: None,
            content,
            tool_calls: Vec::new(),
            tool_call_id: None,
            provider_state: None,
            is_error: false,
            created_at: String::new(),
        });
    }
    // The summary target is prose guidance, not a hard combined thinking/text
    // ceiling. Preserve the main allowance unless the summary role sets one.
    let (roles, _) = models.current_models();
    request.max_output_tokens = roles
        .summary
        .max_output_tokens
        .or(request.max_output_tokens);
    request.session_id.clear();
    request.history = Default::default();
    let admission::ForkSize {
        input_bytes,
        tokens,
        output_reserve,
        window,
    } = admission::admit_fork(models, fork, &input.session_id, &request)?;
    if let Some(trace) = &models.trace {
        trace.runtime_info(json!({
                "event":"episode_summary_input", "mode":"fork", "session_id":input.session_id,
                "model":request.model,"dispatched":fork.dispatched,"input_bytes":input_bytes,
                "estimated_input_tokens":tokens,"output_reserve_tokens":output_reserve,
                "context_window":window,"context_window_known":window.is_some(),"source_first":fork.first_message_id,"source_last":fork.last_message_id,
            }));
    }
    let model = request
        .model
        .split_once('/')
        .map(|(_, model)| model)
        .unwrap_or(&request.model)
        .to_owned();
    let cold = models
        .context_route
        .fork_cold(&input.session_id, &fork.request)
        .into_iter()
        .filter(|index| *index < fork_point)
        .collect::<Vec<_>>();
    // The summary control stays as measured: system plus return control after
    // the parent prefix, with the parent's cold and previous-tail breakpoints.
    let (response, usage) = dispatch_fork(
        models,
        fork,
        &input.session_id,
        ForkCall {
            delivery,
            request,
            fork_point,
            cold: &cold,
            task: None,
            lineage: "summary_fork",
            kind: "summary_fork",
            model: &model,
            attempt,
        },
        &|text| summary_output_reason("summary_fork", text).is_none(),
    )
    .await?;
    let reasoning = response
        .provider_state
        .as_ref()
        .and_then(ProviderRegistry::reasoning);
    let response = ChatResult {
        text: response.text,
        model,
        usage,
        reasoning,
    };
    models.validate_summary_output_at(
        "summary_fork",
        &input.session_id,
        &response,
        &response.text,
        attempt,
    )?;
    let (title, _) = summary_parts(&response.text, input.turn_request.as_deref());
    Ok(SummaryResult {
        title,
        summary: response.text,
        model: response.model,
        usage: response.usage,
        reasoning: response.reasoning,
    })
}

/// The tool result a fork's model gets when it calls a tool anyway.
pub(super) const TOOL_REFUSAL: &str = "No tools in this task; answer in text.";

pub(super) struct ForkCall<'a> {
    pub delivery: ForkDelivery,
    /// The parent's request cut or extended to `fork_point`, plus any side
    /// messages after it.
    pub request: ModelRequest,
    pub fork_point: usize,
    pub cold: &'a [usize],
    /// Appended as a tail reminder before the first dispatch.
    pub task: Option<Reminder>,
    /// The context lineage the fork is recorded under.
    pub lineage: &'a str,
    /// The call kind for traces and output failures.
    pub kind: &'a str,
    pub model: &'a str,
    pub attempt: usize,
}

/// Send a fork of the parent prefix. The fork keeps the parent's tools so the
/// prefix stays byte-identical; a tool call is answered with an error result and
/// retried once, and a second tool call fails as `tool_call_returned`. A missing
/// text answer has its own single recovery retry. Usage covers every attempt;
/// each attempt is measured against the prompt cache.
pub(super) async fn dispatch_fork(
    models: &HttpEpisodeModels,
    fork: &EpisodeFork,
    session: &str,
    call: ForkCall<'_>,
    valid: &(dyn Fn(&str) -> bool + Sync),
) -> Result<(ModelResponse, Usage)> {
    let ForkCall {
        delivery,
        mut request,
        fork_point,
        cold,
        mut task,
        lineage,
        kind,
        model,
        attempt,
    } = call;
    let mut usage = Usage::default();
    let mut tool_retry_used = false;
    let mut empty_retry_used = false;
    loop {
        let sent =
            models
                .context_route
                .fork(request, session, fork_point, lineage, cold, task.take());
        // Summary forks use RequestKind::SummaryFork, so they never declare or
        // cause a main-loop break.
        let cache_pending = models.trace.as_ref().and_then(|_| {
            let shape = fork.provider.wire_shape(&sent)?;
            let id = uuid::Uuid::new_v4().to_string();
            let cache = models.context_route.cache();
            cache.start(
                &id,
                session,
                RequestKind::SummaryFork,
                shape,
                request_components(&sent),
            );
            Some((cache, id))
        });
        request = sent.clone();
        let started = Instant::now();
        let response = fork
            .provider
            .generate_summary_fork(sent, CancellationToken::new())
            .await;
        let observed_usage = response
            .as_ref()
            .ok()
            .and_then(|response| response.usage.clone())
            .or_else(|| {
                response
                    .as_ref()
                    .err()
                    .and_then(bubblepaw_core::ProviderError::of)
                    .and_then(|error| error.usage.clone())
            });
        let this = observed_usage.clone().unwrap_or_default();
        usage.add(&this);
        let selected_route = response
            .as_ref()
            .ok()
            .and_then(|response| response.provider_state.as_ref())
            .and_then(|state| state["_bubblepaw_selected_model"].as_str());
        // Without usage there is nothing to attribute, so the pending entry is dropped.
        let cache_outcome = cache_pending.and_then(|(cache, id)| match &observed_usage {
            Some(usage) => cache.finish(&id, Some(usage), selected_route, true),
            None => {
                cache.abandon(&id);
                None
            }
        });
        models.context_route.finish(
            session,
            lineage,
            selected_route
                .and_then(|route| route.split_once('/'))
                .map_or("", |(provider, _)| provider),
            &request.model,
            observed_usage.as_ref(),
            response
                .as_ref()
                .ok()
                .and_then(bubblepaw_core::RequestLogRow::reported_breakpoints),
        );
        let mut response = response;
        let delivered = response
            .as_mut()
            .is_ok_and(|response| delivery.extract(response));
        let accepted = delivered
            && response
                .as_ref()
                .is_ok_and(|response| response.tool_calls.is_empty() && valid(&response.text));
        trace_call(
            models.trace.as_ref(),
            kind,
            model,
            &this,
            started,
            if accepted { "ok" } else { "error" },
            cache_outcome.as_ref().map(|outcome| outcome.metrics()),
        );
        let response = response.map_err(|error| with_episode_usage(error, usage.clone()))?;
        if delivered {
            if !response.text.trim().is_empty() || empty_retry_used {
                return Ok((response, usage));
            }
            empty_retry_used = true;
            request
                .messages
                .push(super::chat_requests::empty_summary_retry(session));
        } else if tool_retry_used {
            return Err(models.output_failure(
                kind,
                session,
                model,
                ("tool_call_returned", None),
                attempt,
                usage,
            ));
        } else {
            tool_retry_used = true;
            refuse_tool_calls(&mut request, session, response, &delivery.refusal());
        }
        admission::admit_fork(models, fork, session, &request)
            .map_err(|error| with_episode_usage(error, usage.clone()))?;
    }
}

/// Answer every tool call with an error result, keeping the side transcript
/// valid for the retry.
fn refuse_tool_calls(
    request: &mut ModelRequest,
    session: &str,
    response: ModelResponse,
    refusal: &str,
) {
    let message = |role, content: String| Message {
        id: String::new(),
        session_id: session.to_owned(),
        role,
        origin: None,
        content,
        tool_calls: Vec::new(),
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: String::new(),
    };
    request.messages.push(Message {
        tool_calls: response.tool_calls.clone(),
        provider_state: response.provider_state,
        ..message(Role::Assistant, response.text)
    });
    for call in response.tool_calls {
        request.messages.push(Message {
            tool_call_id: Some(call.id),
            is_error: true,
            ..message(Role::Tool, refusal.into())
        });
    }
}
