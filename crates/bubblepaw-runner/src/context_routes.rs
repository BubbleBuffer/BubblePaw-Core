//! Runner-owned auxiliary request assembly. Transport wrappers only forward built requests.
use bubblepaw_core::{
    BuiltContext, CacheAccounting, ContextState, ModelRequest, ModelResponse, Observation,
    ObservationRecord, ObservationScope, Reminder, Store, Usage,
};
use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
};

tokio::task_local! {
    static ACTIVE_ROUTE: ContextRoute;
}

type ForkAnnotations = HashMap<String, Vec<(String, Vec<usize>)>>;

#[derive(Clone)]
pub(crate) struct ContextRoute {
    anonymous_session: Arc<str>,
    /// The host store's ledger once attached; a private one until then. Shared by clones.
    ledger: Arc<Mutex<Arc<CacheAccounting>>>,
    /// Trace events only; the ledger never comes from here.
    observation: Arc<Mutex<Option<Observation>>>,
    forks: Arc<Mutex<ForkAnnotations>>,
}

impl Default for ContextRoute {
    fn default() -> Self {
        Self {
            anonymous_session: format!("auxiliary:{}", uuid::Uuid::new_v4()).into(),
            ledger: Default::default(),
            observation: Default::default(),
            forks: Default::default(),
        }
    }
}

impl ContextRoute {
    pub(crate) fn set_observation(&self, observation: Option<Observation>) {
        *self.observation.lock().unwrap() = observation;
    }
    pub(crate) fn observation(&self) -> Option<Observation> {
        self.observation.lock().unwrap().clone()
    }
    /// The prompt-cache ledger this route's requests are classified against.
    pub(crate) fn cache(&self) -> Arc<CacheAccounting> {
        self.ledger.lock().unwrap().clone()
    }

    /// Context follows a role's async retry chain without changing provider APIs.
    pub(crate) async fn scope<T>(&self, future: impl Future<Output = T>) -> T {
        ACTIVE_ROUTE.scope(self.clone(), future).await
    }

    #[cfg(any(test, feature = "bench"))]
    pub(crate) fn build(
        &self,
        request: ModelRequest,
        session: &str,
        kind: &str,
        cold: &[usize],
    ) -> ModelRequest {
        // Legacy session-less adapter calls share only their owned adapter instance.
        // This diagnostic identity never enters the request or provider transport.
        let session = if session.is_empty() {
            &self.anonymous_session
        } else {
            session
        };
        self.built(session, |context| {
            context.build(request, session, kind, cold)
        })
        .request
    }

    /// A same-route side call that continues `session`'s root prefix. Its first
    /// `fork_point` messages are the parent's last request (cut or extended);
    /// the tiers it returns let the provider read the parent's cache.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn fork(
        &self,
        request: ModelRequest,
        session: &str,
        fork_point: usize,
        kind: &str,
        cold: &[usize],
        task: Option<Reminder>,
    ) -> ModelRequest {
        let built = self.built(session, |context| {
            context.fork(request, session, "root", fork_point, kind, cold, task)
        });
        let mut request = built.request;
        request.cache_tiers = Some(built.tiers);
        request
    }

    /// A fresh side call, declared as not a fork of the session's root prefix.
    pub(crate) fn build_side(
        &self,
        request: ModelRequest,
        session: &str,
        kind: &str,
        reason: Option<&str>,
    ) -> ModelRequest {
        let session = if session.is_empty() {
            &self.anonymous_session
        } else {
            session
        };
        self.built(session, |context| {
            context.build_side(request, session, kind, &[], "root", reason)
        })
        .request
    }

    fn built(
        &self,
        session: &str,
        build: impl FnOnce(&ContextState) -> BuiltContext,
    ) -> BuiltContext {
        let observation = self.observation();
        let built = build(&self.cache().context);
        if let Some(observation) = &observation {
            observation.context_built(
                ObservationScope {
                    session_id: session.into(),
                    model_request_id: Some(uuid::Uuid::new_v4().to_string()),
                    ..Default::default()
                },
                &built.record,
            );
        }
        built
    }

    /// Side calls use `store`'s ledger and log to its request log, traced or not.
    pub(crate) fn attach_store(&self, store: &Store) {
        let ledger = store.cache_accounting();
        ledger.context.attach_request_log(store);
        *self.ledger.lock().unwrap() = ledger;
    }

    /// The route a call inside `scope` builds on: the scoped one, else this.
    /// Deciders build on it as fresh side calls against the root prefix.
    pub(crate) fn inherited(&self) -> Self {
        ACTIVE_ROUTE
            .try_with(Clone::clone)
            .unwrap_or_else(|_| self.clone())
    }

    /// Close a side call's row with the response's usage and selected provider.
    pub(crate) fn finish_response(
        &self,
        session: &str,
        lineage: &str,
        model: &str,
        response: &ModelResponse,
    ) {
        let provider = crate::provider::ProviderRegistry::selected_model(response)
            .and_then(|selected| selected.split_once('/'))
            .map_or("", |(provider, _)| provider);
        let breakpoints = bubblepaw_core::RequestLogRow::reported_breakpoints(response);
        let usage = response.usage.as_ref();
        self.finish(session, lineage, provider, model, usage, breakpoints);
    }

    /// Close a side lineage's request-log row with its usage. An unexplained
    /// cache miss goes to the trace as `cache_miss_unexplained`.
    pub(crate) fn finish(
        &self,
        session: &str,
        lineage: &str,
        provider: &str,
        model: &str,
        usage: Option<&Usage>,
        breakpoints: Option<Vec<String>>,
    ) {
        let session = if session.is_empty() {
            &self.anonymous_session
        } else {
            session
        };
        let observation = self.observation();
        let warning = self.cache().context.finish_request(
            session,
            lineage,
            provider,
            model,
            usage,
            breakpoints,
        );
        if let (Some(metrics), Some(observation)) = (warning, &observation) {
            observation.sink.record(ObservationRecord {
                created_at: chrono::Utc::now().to_rfc3339(),
                kind: "cache_miss_unexplained".into(),
                scope: ObservationScope {
                    session_id: session.into(),
                    ..Default::default()
                },
                metrics,
                payload: None,
                payload_truncated: false,
            });
        }
    }

    /// Captures carry annotations in a disposable sidecar, preserving EpisodeFork's API.
    pub(crate) fn record_fork(&self, session: &str, request: &ModelRequest, cold: &[usize]) {
        let key = request_key(request);
        let mut forks = self.forks.lock().unwrap();
        let rows = forks.entry(session.into()).or_default();
        rows.retain(|(existing, _)| existing != &key);
        rows.push((key, cold.to_vec()));
        if rows.len() > 16 {
            rows.remove(0);
        }
    }

    pub(crate) fn fork_cold(&self, session: &str, request: &ModelRequest) -> Vec<usize> {
        let key = request_key(request);
        self.forks
            .lock()
            .unwrap()
            .get(session)
            .and_then(|rows| rows.iter().rev().find(|(existing, _)| existing == &key))
            .map(|(_, cold)| cold.clone())
            .unwrap_or_default()
    }
}

fn request_key(request: &ModelRequest) -> String {
    bubblepaw_core::fingerprint(
        &serde_json::to_vec(request).expect("model request is JSON serializable"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bubblepaw_core::{Message, ObservationMode, ObservationRecord, ObservationSink, Role};

    #[derive(Default)]
    struct Sink(Mutex<Vec<ObservationRecord>>);
    impl ObservationSink for Sink {
        fn record(&self, record: ObservationRecord) {
            self.0.lock().unwrap().push(record);
        }
    }
    fn observed() -> (ContextRoute, Arc<Sink>) {
        let sink = Arc::new(Sink::default());
        let observation = Observation {
            sink: sink.clone(),
            mode: ObservationMode::Contents,
            max_payload_bytes: usize::MAX,
        };
        (
            ContextRoute {
                observation: Arc::new(Mutex::new(Some(observation))),
                ..Default::default()
            },
            sink,
        )
    }
    fn request() -> ModelRequest {
        ModelRequest {
            system_prompt: "frozen-secret".into(),
            messages: vec![Message {
                role: Role::User,
                content: "history-secret".into(),
                provider_state: Some(serde_json::json!({"signature":"native-secret"})),
                id: String::new(),
                session_id: String::new(),
                origin: None,
                tool_calls: Vec::new(),
                tool_call_id: None,
                is_error: false,
                created_at: String::new(),
            }],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn inherited_route_builds_once_and_keeps_child_and_operation_lineages_separate() {
        let (parent, sink) = observed();
        let fallback = ContextRoute::default();
        let request = request();
        let before = serde_json::to_vec(&request).unwrap();
        parent
            .scope(async {
                let built =
                    fallback
                        .inherited()
                        .build_side(request.clone(), "root", "decider", None);
                assert_eq!(serde_json::to_vec(&built).unwrap(), before);
                fallback
                    .inherited()
                    .build_side(request.clone(), "root", "decider", None);
                fallback
                    .inherited()
                    .build_side(request.clone(), "child", "decider", None);
                fallback
                    .inherited()
                    .build_side(request.clone(), "root", "chapter", None);
            })
            .await;
        let records = sink.0.lock().unwrap();
        assert_eq!(records.len(), 4);
        assert!(
            records
                .iter()
                .all(|r| r.kind == "context_built" && r.payload.is_none())
        );
        assert_eq!(records[0].metrics["classification"]["kind"], "first");
        assert_eq!(records[1].metrics["classification"]["kind"], "append");
        assert_eq!(records[2].metrics["classification"]["kind"], "first");
        assert_eq!(records[3].metrics["classification"]["kind"], "first");
        let serialized = serde_json::to_string(&*records).unwrap();
        assert!(!serialized.contains("secret"));
    }

    #[test]
    fn untraced_side_calls_are_logged_with_their_usage() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_data_dir(dir.path()).unwrap();
        let route = ContextRoute::default();
        route.attach_store(&store);
        route.build_side(request(), "s", "decider:choice", None);
        let response = ModelResponse {
            usage: Some(Usage {
                input_tokens: 40,
                cache_read_tokens: 30,
                ..Default::default()
            }),
            provider_state: Some(serde_json::json!({"_bubblepaw_selected_model": "go/m"})),
            ..Default::default()
        };
        route.finish_response("s", "decider:choice", "m", &response);
        let rows = store.model_requests("s").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].lineage, "decider:choice");
        assert_eq!(rows[0].provider, "go");
        assert_eq!(rows[0].cached_tokens, Some(30));
        assert_eq!(rows[0].uncached_tokens, Some(10));
    }

    #[test]
    fn immutable_fork_annotations_follow_exact_capture() {
        let route = ContextRoute::default();
        let request = request();
        route.record_fork("root", &request, &[0]);
        assert_eq!(route.fork_cold("root", &request), [0]);
        assert!(route.fork_cold("child", &request).is_empty());
        let mut changed = request.clone();
        changed.messages[0].content.push_str(" next capture");
        assert!(route.fork_cold("root", &changed).is_empty());
    }
    #[test]
    fn sessionless_adapter_instances_have_separate_diagnostic_lineage() {
        let (first, sink) = observed();
        let second = ContextRoute {
            ledger: first.ledger.clone(),
            observation: first.observation.clone(),
            ..Default::default()
        };
        let result = first.build(request(), "", "decider:approve", &[]);
        assert!(result.session_id.is_empty());
        second.build(request(), "", "decider:approve", &[]);
        let records = sink.0.lock().unwrap();
        assert_ne!(records[0].scope.session_id, records[1].scope.session_id);
        assert!(
            records
                .iter()
                .all(|r| r.metrics["classification"]["kind"] == "first")
        );
    }
}
