//! Benchmark transport instrumentation preserves configured route identity guards.
use super::*;

impl ProviderRegistry {
    /// Wrap each configured transport while preserving model/account/provenance
    /// policy. Runtime sessions, quota hosts and usage stores start independently.
    /// Fallbacks are deliberately omitted so each benchmark arm keeps its model.
    pub(crate) fn map_benchmark_transports<F, D>(
        &self,
        mut provider: F,
        mut decision: D,
    ) -> Result<Self>
    where
        F: FnMut(&str, Arc<dyn Provider>) -> Result<Arc<dyn Provider>>,
        D: FnMut(&str, Arc<dyn DecisionProvider>) -> Result<Arc<dyn DecisionProvider>>,
    {
        let mut mapped = Self::new(
            &self.default,
            provider(
                &self.default,
                self.get(&self.default).expect("default provider"),
            )?,
        );
        for (id, original) in &self.providers {
            if id != &self.default {
                mapped.insert(id, provider(id, original.clone())?)?;
            }
        }
        for (id, original) in &self.decisions {
            mapped.insert_decision(id, decision(id, original.clone())?)?;
        }
        mapped.origins = self.origins.clone();
        mapped.configured_models = self.configured_models.clone();
        mapped.removed_models = self.removed_models.clone();
        mapped.strict_routes = self.strict_routes;
        mapped.image_input = self.image_input.clone();
        mapped.accounts = self.accounts.clone();
        mapped.context_route = self.context_route.clone();
        mapped.retry = self.retry.clone();
        Ok(mapped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Decision;
    #[async_trait]
    impl DecisionProvider for Decision {
        async fn decide(
            &self,
            _: DecisionRequest,
            _: CancellationToken,
        ) -> Result<DecisionResponse> {
            anyhow::bail!("Metadata regression must not make provider calls")
        }
    }

    #[test]
    fn mapped_transports_preserve_removed_models_and_endpoint_replay_guards() -> Result<()> {
        let original = Arc::new(crate::provider::ScriptedProvider::new("unused"));
        let mut source = ProviderRegistry::new("main", original.clone()).with_strict_routes();
        source.insert("secondary", original)?;
        source.insert_decision("choices", Arc::new(Decision))?;
        source.configure_route(
            "main",
            "https://expected.invalid".into(),
            BTreeSet::from(["current".into()]),
        );
        source.set_removed_models(BTreeSet::from([
            "main/removed".into(),
            "choices/removed".into(),
        ]));
        source.set_image_input("main", "current", true);
        source.set_account("main", "account-one");
        source.set_retry_policy(RetryPolicy::once());
        source.set_model_fallbacks("main/current".into(), vec!["secondary/alternate".into()])?;
        source
            .last
            .lock()
            .unwrap()
            .insert("live-session".into(), "main".into());
        let mut provider_ids = Vec::new();
        let mut decision_ids = Vec::new();
        let mapped = source.map_benchmark_transports(
            |id, provider| {
                provider_ids.push(id.to_owned());
                Ok(provider)
            },
            |id, decision| {
                decision_ids.push(id.to_owned());
                Ok(decision)
            },
        )?;
        assert_eq!(provider_ids, vec!["main", "secondary"]);
        assert_eq!(decision_ids, vec!["choices"]);
        assert!(mapped.checked_route("main/removed").is_err());
        assert!(!mapped.decision_available("choices/removed"));
        assert!(mapped.checked_route("missing/current").is_err());
        assert!(mapped.checked_route("main/current").is_ok());
        assert_eq!(mapped.origins, source.origins);
        assert_eq!(mapped.configured_models, source.configured_models);
        assert_eq!(mapped.image_input, source.image_input);
        assert_eq!(mapped.account("main/current"), "account-one");
        assert_eq!(mapped.retry.max_attempts, 1);
        assert!(mapped.last.lock().unwrap().is_empty());
        assert!(mapped.selected.lock().unwrap().is_empty());
        assert!(mapped.auxiliary_selected.lock().unwrap().is_empty());
        assert!(mapped.fallbacks.is_empty());
        assert!(mapped.quota_host.lock().unwrap().is_none());
        assert!(mapped.usage_store.lock().unwrap().is_none());
        let mut request = ModelRequest {
            messages: vec![serde_json::from_value(json!({
                "id":"assistant-source","session_id":"source","role":"assistant",
                "content":"Synthetic recorded reply","created_at":"",
                "provider_state":{
                    "_bubblepaw_route_origin":{"provider":"main","endpoint":"https://old.invalid"},
                    (bubblepaw_core::PROVIDER_STATE_ORIGIN_FIELD):{"provider_kind":"chat-completions"}
                }
            }))?],
            ..Default::default()
        };
        let mut expected = request.clone();
        source.prepare_replay("main", &mut expected);
        mapped.prepare_replay("main", &mut request);
        assert_eq!(request.messages, expected.messages);
        assert_eq!(
            request.messages[0].provider_state.as_ref().unwrap()
                [bubblepaw_core::PROVIDER_STATE_ORIGIN_FIELD]["provider_kind"],
            "incompatible-provider-route"
        );
        Ok(())
    }
}
