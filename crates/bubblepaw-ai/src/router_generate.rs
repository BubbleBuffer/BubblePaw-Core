//! The dispatch loop of the router: candidates in order, one retry loop per
//! candidate, the stop rule, and the sticky selection after a success.
use crate::{
    CancellationToken, LastRequestUsage, ModelRequest, ModelResponse, ProviderError,
    StreamCallback, Usage,
    adapter::{
        RetryPolicy, add_usage, cancelled, is_terminal_quota_error, mark_published,
        retain_prior_quota, with_retry,
    },
    is_quota_stop,
    router::Router,
    router_observer::{BoxFuture, RouteObserver, RouteRef},
    router_reply::{FailedAttempts, fallback_reason, stamp_route, with_usage},
    router_stream::{discard_abandoned, guard},
};
use anyhow::Result;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

impl Router {
    /// Try the primary route and its configured fallbacks in order. A session's
    /// sticky route starts the list; the choice is written only after a success.
    /// `retry_base_delay` replaces the policy's initial delay for this call.
    pub async fn generate_routed(
        &self,
        request: ModelRequest,
        on_text: StreamCallback,
        cancellation: CancellationToken,
        decision: bool,
        retry_base_delay: Option<Duration>,
        observer: &dyn RouteObserver,
    ) -> Result<ModelResponse> {
        let primary = self
            .checked_route(&request.model)
            .map(|(id, _, model)| format!("{id}/{model}"))
            .unwrap_or_else(|_| request.model.clone());
        let logical_session = request.session_id.clone();
        // Stateless auxiliary work can reuse the confirmed main route, but its
        // own fallbacks must never switch that route or a main-session mirror.
        // Its source message still supplies the wire header.
        let source_session = if logical_session.is_empty() {
            request
                .messages
                .iter()
                .rev()
                .find(|message| !message.session_id.is_empty())
                .map(|message| message.session_id.clone())
                .unwrap_or_default()
        } else {
            logical_session.clone()
        };
        let key = (source_session.clone(), primary.clone());
        let selections = if logical_session.is_empty() {
            &self.auxiliary_selected
        } else {
            &self.selected
        };
        let mut candidates = self.chain(&primary);
        let has_fallbacks = candidates.len() > 1;
        let selected = selections.lock().unwrap().get(&key).cloned().or_else(|| {
            logical_session
                .is_empty()
                .then(|| self.selected.lock().unwrap().get(&key).cloned())
                .flatten()
        });
        if !source_session.is_empty()
            && let Some(selected) = selected
            && let Some(index) = candidates
                .iter()
                .position(|candidate| candidate == &selected)
        {
            candidates.drain(..index);
        }
        let mut failed = Vec::new();
        // Classified message per failed route (e.g. a subscription usage limit
        // with its type and reset time); never raw provider text.
        let mut reasons = Vec::new();
        let mut accumulated: Option<Usage> = None;
        let mut last_error = None;
        // Auxiliary role work keeps its own base delay; everything else uses the
        // router policy.
        let policy = match retry_base_delay {
            Some(base) => RetryPolicy {
                initial_delay: base,
                ..self.retry_policy().clone()
            },
            None => self.retry_policy().clone(),
        };
        for candidate in candidates {
            if cancellation.is_cancelled() {
                return Err(with_usage(cancelled(), accumulated.as_ref(), false));
            }
            let (id, provider, model) = match self.checked_route(&candidate) {
                Ok(route) => route,
                Err(error) => {
                    failed.push(candidate);
                    reasons.push("route unavailable".to_owned());
                    last_error = Some(error);
                    continue;
                }
            };
            let mut next = request.clone();
            next.model = model.clone();
            self.prepare_replay(&id, &mut next);
            if !logical_session.is_empty() {
                let previous = self
                    .last
                    .lock()
                    .unwrap()
                    .insert(logical_session.clone(), id.clone());
                // Cursor mirrors belong to the actual provider. A model change
                // on that provider preserves history; fallback selection has
                // its own source-session/model key above.
                if let Some(previous) = previous.filter(|previous| previous != &id) {
                    if let Some(old) = self.get(&previous) {
                        let _ = old.close_session(&logical_session).await;
                    }
                    self.resets.reset(&logical_session, &mut next.history);
                } else {
                    self.resets.apply(&logical_session, &mut next.history);
                }
            }
            let published = Arc::new(AtomicBool::new(false));
            let visible = published.clone();
            let (guarded, thinking) = guard(on_text.clone(), visible);
            let attempt_state = Arc::new(Mutex::new(FailedAttempts::default()));
            let route = RouteRef {
                provider: &id,
                model: &model,
            };
            let call = || {
                let request = next.clone();
                let on_text = guarded.clone();
                let (discard, thinking) = (on_text.clone(), thinking.clone());
                let cancellation = cancellation.clone();
                let published = published.clone();
                let provider = provider.clone();
                async move {
                    let run: BoxFuture<'_, Result<ModelResponse>> = Box::pin(async move {
                        let result = if decision {
                            provider
                                .generate_decision(request, on_text, cancellation)
                                .await
                        } else {
                            provider.generate(request, on_text, cancellation).await
                        };
                        let result = result.map_err(|error| {
                            mark_published(error, published.load(Ordering::Acquire))
                        });
                        discard_abandoned(&result, &published, &thinking, &discard);
                        result
                    });
                    observer.attempt(route, run).await
                }
            };
            let on_failed_attempt = |error: &anyhow::Error| {
                attempt_state.lock().unwrap().record(error);
            };
            let hosted_policy = RetryPolicy {
                yield_rate_limits: observer.yield_rate_limits(),
                ..policy.clone()
            };
            let mut retry = || {
                let run: BoxFuture<'_, Result<ModelResponse>> = Box::pin(with_retry(
                    &id,
                    &hosted_policy,
                    &cancellation,
                    &published,
                    &call,
                    &on_failed_attempt,
                ));
                run
            };
            let result = observer.candidate(route, &cancellation, &mut retry).await;
            let attempt_state = std::mem::take(&mut *attempt_state.lock().unwrap());
            if let Some(usage) = &attempt_state.usage {
                add_usage(&mut accumulated, usage);
            }
            let result = match result {
                Ok(mut response) => {
                    if !attempt_state.quota.is_empty() {
                        let mut quota = attempt_state.quota;
                        quota.extend(response.quota);
                        response.quota = quota;
                    }
                    Ok(response)
                }
                Err(error) => Err(retain_prior_quota(error, attempt_state.quota)),
            };
            match result {
                Ok(mut response) => {
                    if accumulated.is_some() && response.last_request_usage.is_none() {
                        response.last_request_usage = Some(LastRequestUsage {
                            input_tokens: response.usage.as_ref().map(|usage| usage.input_tokens),
                            output_tokens: response.usage.as_ref().map(|usage| usage.output_tokens),
                        });
                    }
                    if let Some(usage) = &response.usage {
                        add_usage(&mut accumulated, usage);
                    }
                    response.usage = accumulated;
                    self.record_origin(&id, &mut response);
                    if has_fallbacks {
                        stamp_route(&mut response, &candidate, &failed, &reasons);
                        // A fallback route also has provenance when its adapter
                        // returned no native replay state.
                        self.record_origin(&id, &mut response);
                        if !source_session.is_empty() {
                            selections.lock().unwrap().insert(key, candidate);
                        }
                    }
                    return Ok(response);
                }
                Err(error) => {
                    if let Some(usage) =
                        ProviderError::of(&error).and_then(|error| error.usage.as_ref())
                    {
                        add_usage(&mut accumulated, usage);
                    }
                    let published = published.load(Ordering::Acquire)
                        || ProviderError::of(&error).is_some_and(|error| error.published);
                    let stopped = cancellation.is_cancelled()
                        || ProviderError::of(&error)
                            .is_some_and(|error| error.message == "cancelled");
                    if published
                        || stopped
                        || is_quota_stop(&error)
                        || ProviderError::of(&error).is_some_and(is_terminal_quota_error)
                    {
                        return Err(with_usage(error, accumulated.as_ref(), published));
                    }
                    let reason = fallback_reason(&error);
                    observer.route_failed(&candidate, &reason);
                    failed.push(candidate);
                    reasons.push(reason);
                    last_error = Some(error);
                }
            }
        }
        Err(with_usage(
            last_error.unwrap_or_else(|| anyhow::anyhow!("No configured model route is available")),
            accumulated.as_ref(),
            false,
        ))
    }
}
