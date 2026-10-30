//! Replay probe transport. Harness answer rounds arrive already built.
use super::*;

pub(super) struct ReplayProbeModel {
    pub(super) context_route: crate::context_routes::ContextRoute,
    pub(super) telemetry: Arc<Mutex<Telemetry>>,
    pub(super) prices: &'static PriceTable,
    pub(super) url: Option<String>,
    pub(super) client: reqwest::Client,
    pub(super) model: String,
    pub(super) effort: ReasoningEffort,
    pub(super) registry: Option<Arc<ProviderRegistry>>,
}
#[async_trait]
impl crate::probes::ProbeModel for ReplayProbeModel {
    fn model(&self) -> &str {
        &self.model
    }
    fn reasoning_effort(&self) -> ReasoningEffort {
        self.effort
    }
    async fn close_session(&self, session_id: &str) -> Result<()> {
        match &self.registry {
            Some(registry) => registry.close_session(session_id).await,
            None => Ok(()),
        }
    }
    async fn call(&self, body: Value, kind: &str) -> Result<Value> {
        self.call_with_lineage(body, kind, "").await
    }
    async fn call_in_session(&self, body: Value, kind: &str, session: &str) -> Result<Value> {
        self.call_with_lineage(body, kind, session).await
    }
}
impl ReplayProbeModel {
    async fn call_with_lineage(&self, mut body: Value, kind: &str, session: &str) -> Result<Value> {
        {
            let mut meter = self.telemetry.lock().unwrap();
            meter.guard()?;
            meter.begin_operation(kind);
        }
        // Answer rounds were built by Harness. Judges have a standalone assembly;
        // raw Chat transport uses its unchanged original body after diagnostics.
        let request = if self.registry.is_some() || kind != "probe" {
            let request = crate::probes::provider_request(&body, &self.model, self.effort)?;
            Some(if kind == "probe" {
                request
            } else {
                self.context_route.build(request, session, kind, &[])
            })
        } else {
            None
        };
        if let Some(registry) = &self.registry {
            let request = request.expect("registry request was assembled");
            let response = registry
                .generate(request, Arc::new(|_| {}), CancellationToken::new())
                .await?;
            return Ok(crate::probes::provider_payload(response));
        }
        body["model"] = json!(
            self.model
                .split_once('/')
                .context("Unqualified probe model")?
                .1
        );
        body["reasoning_effort"] = json!(self.effort.as_str());
        body.as_object_mut().unwrap().remove("_bubblepaw_request");
        for message in body["messages"].as_array_mut().into_iter().flatten() {
            if let Some(message) = message.as_object_mut() {
                for key in ["id", "session_id", "origin", "is_error"] {
                    message.remove(key);
                }
            }
        }
        if self.model == FLASH_MODEL {
            for message in body["messages"].as_array_mut().into_iter().flatten() {
                if let Some(message) = message.as_object_mut() {
                    message.remove("provider_state");
                    message.remove("is_error");
                }
            }
        }
        if let Some(url) = &self.url {
            let response = self.client.post(url).json(&body).send().await?;
            ensure!(
                response.status().is_success(),
                "Metered probe request failed"
            );
            return Ok(response.json().await?);
        }
        // Deterministic synthetic traffic exercises one real session-local history
        // round and judge parsing. These grades are plumbing proof, never quality.
        let has_tool_result = body["messages"].as_array().is_some_and(|messages| {
            messages
                .iter()
                .any(|m| m["tool_call_id"] == "scripted-history-search")
        });
        let message = if kind == "judge" {
            json!({"role":"assistant","content":"{\"grade\":\"supported\",\"reason\":\"Scripted plumbing grade; no recall quality claim.\"}"})
        } else if !has_tool_result && body["tools"].is_array() {
            json!({"role":"assistant","content":null,"tool_calls":[{"id":"scripted-history-search","type":"function","function":{"name":"history_search","arguments":"{\"query\":\"Garden\",\"limit\":1}"}}]})
        } else {
            json!({"role":"assistant","content":"Scripted probe answer after session-local history lookup. No model recall quality measured."})
        };
        let input = token_count(&body.to_string())? as u64;
        let output = token_count(&message.to_string())? as u64;
        let mut meter = self.telemetry.lock().unwrap();
        let operation = meter.current_operation;
        let projected = self
            .prices
            .usage_cost(
                &self.model,
                &Usage {
                    input_tokens: input,
                    output_tokens: output,
                    ..Default::default()
                },
                None,
            )
            .context("Unpriced probe model")?;
        meter.calls.push(json!({"kind":kind,"model":"scripted","model_address":self.model,"operation":operation,"physical_call_index":1,"request_slot":1,"attempt":1,"transport_retry":false,"outcome":"scripted","input_tokens":input,"output_tokens":output,"reasoning_tokens":0,"cost_usd":0.0,"projected_paid_cost_usd":projected,"latency_ms":0}));
        Ok(
            json!({"choices":[{"message":message,"finish_reason":"stop"}],"usage":{"prompt_tokens":input,"completion_tokens":output,"completion_tokens_details":{"reasoning_tokens":0}}}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probes::ProbeModel;
    use axum::{Json, Router, extract::State, routing::post};

    #[tokio::test]
    async fn raw_judge_lineage_preserves_transport_body_and_builds_once() -> Result<()> {
        type Bodies = Arc<Mutex<Vec<Value>>>;
        async fn reply(State(bodies): State<Bodies>, Json(body): Json<Value>) -> Json<Value> {
            bodies.lock().unwrap().push(body);
            Json(json!({"choices":[{"message":{"role":"assistant","content":"ok"}}]}))
        }
        let bodies = Bodies::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let router = Router::new()
            .route("/chat", post(reply))
            .with_state(bodies.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("trace.jsonl");
        let trace = crate::trace::JsonlTrace::open(crate::trace::TraceConfig {
            path: path.clone(),
            ..Default::default()
        })?;
        let context_route = crate::context_routes::ContextRoute::default();
        context_route.set_observation(Some(trace.observation()));
        let model = ReplayProbeModel {
            context_route,
            telemetry: Default::default(),
            prices: PriceTable::shared(),
            url: Some(format!("http://{address}/chat")),
            client: reqwest::Client::new(),
            model: "fixture/judge".into(),
            effort: ReasoningEffort::Low,
            registry: None,
        };
        let body = json!({"messages":[{"role":"system","content":"base"},
            {"role":"user","content":"judge evidence"}],"max_tokens":100});
        let mut expected = body.clone();
        expected["model"] = json!("judge");
        expected["reasoning_effort"] = json!("low");
        model
            .call_in_session(body, "judge", "source-session")
            .await?;
        trace.flush(Duration::from_secs(3))?;
        let status = trace.status();
        assert_eq!(status.dropped, 0);
        assert_eq!(status.write_errors, 0);
        assert_eq!(bodies.lock().unwrap().as_slice(), &[expected]);
        let rows: Vec<Value> = std::fs::read_to_string(path)?
            .lines()
            .map(|line| {
                serde_json::from_str::<Value>(line).map(|envelope| envelope["record"].clone())
            })
            .collect::<std::result::Result<_, _>>()?;
        let built: Vec<_> = rows
            .iter()
            .filter(|row| row["kind"] == "context_built")
            .collect();
        assert_eq!(built.len(), 1);
        assert_eq!(built[0]["session_id"], "source-session");
        server.abort();
        Ok(())
    }
}
