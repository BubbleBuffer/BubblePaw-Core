//! Auxiliary usage and context observations follow the host across generations.
use super::*;

impl ProviderRegistry {
    /// Error for a decision role whose models all lack a decision adapter.
    pub(crate) fn no_decision_model(&self, models: &[String]) -> anyhow::Error {
        let skipped = models
            .iter()
            .filter(|model| !self.decision_available(model))
            .map(String::as_str)
            .collect::<Vec<_>>();
        anyhow::anyhow!(
            "No configured decision model is available; no decision adapter for [{}] (a chat              model serves decisions as llm/main or llm/<provider>/<model>)",
            skipped.join(", ")
        )
    }
    pub(crate) fn set_context_observation(&self, observation: Option<bubblepaw_core::Observation>) {
        self.context_route.set_observation(observation);
    }
    /// The readable reasoning of a reply: its thinking text, or for a reply that
    /// carries it only in the replay state, that state's reasoning.
    pub(crate) fn reply_reasoning(response: &bubblepaw_core::ModelResponse) -> Option<String> {
        if !response.thinking.is_empty() {
            return Some(response.thinking.clone());
        }
        response.provider_state.as_ref().and_then(Self::reasoning)
    }
    /// Legacy: reasoning text a provider returned in its replay state, if any.
    pub(crate) fn reasoning(state: &serde_json::Value) -> Option<String> {
        let mut parts = Vec::new();
        reasoning_parts(state, &mut parts);
        (!parts.is_empty()).then(|| parts.join("\n"))
    }
    /// Side calls share the ledger of the store their usage goes to, and log there.
    pub(super) fn attach_context_log(&self, store: Store) -> Store {
        self.context_route.attach_store(&store);
        store
    }
    pub(crate) fn context_route(&self) -> crate::context_routes::ContextRoute {
        self.context_route.clone()
    }
    pub(crate) fn inherit_context_from(&mut self, previous: &Self) {
        self.context_route = previous.context_route.clone();
    }
    /// Direct role callers also retain usage; episode workers supply an outer
    /// scope so their last result can share the derived-memory transaction.
    pub(crate) async fn auxiliary_usage<F, T>(
        &self,
        session: &str,
        kind: RequestKind,
        future: F,
    ) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        let store = self.usage_store.lock().unwrap().clone();
        if bubblepaw_core::request_usage_count().is_some() || session.is_empty() {
            return future.await;
        }
        let Some(store) = store else {
            return future.await;
        };
        let (result, records) =
            bubblepaw_core::capture_request_usage_with_store(&store, session, None, kind, future)
                .await;
        for record in records {
            store.queue_request_usage(record)?;
        }
        store.flush_request_usage(session)?;
        result
    }
}

/// Read only visible reasoning text; signatures and opaque encrypted state are excluded.
fn reasoning_parts<'a>(state: &'a serde_json::Value, parts: &mut Vec<&'a str>) {
    if let Some(rounds) = state["rounds"].as_array() {
        for round in rounds {
            reasoning_parts(round, parts);
        }
        return;
    }
    for fields in [state, &state["message_fields"]] {
        if let Some(text) = fields["reasoning_content"]
            .as_str()
            .filter(|text| !text.is_empty())
            .or_else(|| fields["reasoning"].as_str().filter(|text| !text.is_empty()))
        {
            parts.push(text);
        }
    }
    for item in state["items"].as_array().into_iter().flatten() {
        if item["type"] == "reasoning" {
            for part in item["summary"].as_array().into_iter().flatten() {
                if let Some(text) = part["text"].as_str() {
                    parts.push(text);
                }
            }
        }
    }
    for item in state["content"].as_array().into_iter().flatten() {
        if item["type"] == "thinking"
            && let Some(text) = item["thinking"].as_str()
        {
            parts.push(text);
        }
    }
}

/// Name the route that served a reply, and the routes that failed before it, as
/// typed fields. The replay state keeps the same keys for readers of older replies.
pub(super) fn stamp_route(
    response: &mut bubblepaw_core::ModelResponse,
    selected: &str,
    failed: &[String],
    reasons: &[String],
) {
    response.route = Some(bubblepaw_core::ServedRoute {
        selected: selected.to_owned(),
        fallback_from: failed.to_vec(),
        fallback_reasons: reasons.to_vec(),
    });
    let state = response
        .provider_state
        .get_or_insert_with(|| serde_json::json!({}));
    if let Some(state) = state.as_object_mut() {
        state.insert(
            "_bubblepaw_selected_model".into(),
            serde_json::json!(selected),
        );
        if !failed.is_empty() {
            state.insert("_bubblepaw_fallback_from".into(), serde_json::json!(failed));
            state.insert(
                "_bubblepaw_fallback_reasons".into(),
                serde_json::json!(reasons),
            );
        }
    }
}
