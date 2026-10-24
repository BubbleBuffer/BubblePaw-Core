use super::*;
struct ForkOnly {
    requests: Mutex<Vec<ModelRequest>>,
}
#[async_trait]
impl Provider for ForkOnly {
    fn supports_summary_fork(&self, _: &str) -> bool {
        true
    }
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        panic!("fork must use its isolated dispatch boundary");
    }
    async fn generate_summary_fork(
        &self,
        request: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request);
        Err(ProviderError::new(ProviderErrorKind::Auth, "branch rejected").into())
    }
}
#[tokio::test]
async fn fork_error_does_not_touch_main_route_or_mirror_bookkeeping_or_try_fallbacks() -> Result<()>
{
    let exact = Arc::new(ForkOnly {
        requests: Mutex::new(Vec::new()),
    });
    let other = Arc::new(ForkOnly {
        requests: Mutex::new(Vec::new()),
    });
    let mut registry = ProviderRegistry::new("free", exact.clone());
    registry.insert("go", other.clone())?;
    registry.set_model_fallbacks("free/flash-free".into(), vec!["go/flash".into()])?;
    registry
        .last
        .lock()
        .unwrap()
        .insert("fixture".into(), "go".into());
    registry.selected.lock().unwrap().insert(
        ("fixture".into(), "free/flash-free".into()),
        "go/flash".into(),
    );
    let before_last = registry.last.lock().unwrap().clone();
    let before_selected = registry.selected.lock().unwrap().clone();
    assert_eq!(
        registry.summary_fork_routes("free/flash-free"),
        vec!["free/flash-free", "go/flash"]
    );
    // An already successful paid main route must not restart the free route.
    assert_eq!(registry.summary_fork_routes("go/flash"), vec!["go/flash"]);
    assert!(exact.requests.lock().unwrap().is_empty());
    assert!(other.requests.lock().unwrap().is_empty());
    assert_eq!(*registry.last.lock().unwrap(), before_last);
    assert_eq!(*registry.selected.lock().unwrap(), before_selected);
    let mut req = request();
    req.model = "free/flash-free".into();
    req.history = bubblepaw_core::HistoryCursor::fresh();
    assert!(
        registry
            .generate_summary_fork(req, CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(*registry.last.lock().unwrap(), before_last);
    assert_eq!(*registry.selected.lock().unwrap(), before_selected);
    assert!(other.requests.lock().unwrap().is_empty());
    let requests = exact.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model, "flash-free");
    assert!(requests[0].session_id.is_empty());
    assert_eq!(
        requests[0].history,
        bubblepaw_core::HistoryCursor::default()
    );
    Ok(())
}
use std::collections::VecDeque;

#[test]
fn cold_resume_estimate_covers_go_reasoning_even_before_free_falls_back() -> Result<()> {
    use crate::provider::ChatCompletionsProvider;
    use bubblepaw_core::{ReasoningReplay, Role};
    use std::collections::BTreeMap;
    let free = Arc::new(
        ChatCompletionsProvider::without_key("http://127.0.0.1:1")?.with_reasoning_replay(
            BTreeMap::from([("flash-free".into(), ReasoningReplay::All)]),
        )?,
    );
    let go = Arc::new(
        ChatCompletionsProvider::without_key("http://127.0.0.1:2")?
            .with_reasoning_replay(BTreeMap::from([("flash".into(), ReasoningReplay::All)]))?,
    );
    let mut registry = ProviderRegistry::new("opencode-free", free.clone());
    registry.insert("opencode", go.clone())?;
    registry.set_model_fallbacks(
        "opencode-free/flash-free".into(),
        vec!["opencode/flash".into()],
    )?;
    let mut req = request();
    req.messages[0].role = Role::Assistant;
    req.messages[0].provider_state = Some(bubblepaw_core::with_provider_state_origin(
        serde_json::json!({"adapter":"chat-completions","message_fields":{"reasoning_content":" thought".repeat(1_000)}}),
        "chat-completions",
        Some("flash"),
        "native",
    )?);
    let count = |value: &serde_json::Value| value.to_string().len();
    let mut direct = req.clone();
    direct.model = "flash".into();
    let paid = go.estimate_input_tokens(&direct, &count)?.unwrap();
    direct.model = "flash-free".into();
    let free_tokens = free.estimate_input_tokens(&direct, &count)?.unwrap();
    assert!(free_tokens < paid);
    assert_eq!(
        registry.estimate_summary_fork_tokens(&req, &count)?,
        Some(free_tokens)
    );
    let mut paid_fork = req.clone();
    paid_fork.model = "opencode/flash".into();
    assert_eq!(
        registry.estimate_summary_fork_tokens(&paid_fork, &count)?,
        Some(paid)
    );
    assert_eq!(registry.estimate_input_tokens(&req, &count)?, Some(paid));
    assert!(
        registry.selected.lock().unwrap().is_empty(),
        "estimation must not select a route"
    );
    registry
        .selected
        .lock()
        .unwrap()
        .insert(("fixture".into(), req.model.clone()), req.model.clone());
    assert_eq!(registry.estimate_input_tokens(&req, &count)?, Some(paid));
    registry.set_removed_models(BTreeSet::from([req.model.clone()]));
    assert_eq!(registry.estimate_input_tokens(&req, &count)?, Some(paid));
    registry
        .fallbacks
        .get_mut(&req.model)
        .unwrap()
        .push("removed/model".into());
    assert_eq!(registry.estimate_input_tokens(&req, &count)?, Some(paid));
    Ok(())
}

#[derive(Clone, Copy)]
enum Step {
    Auth,
    Transient,
    Published,
    Cancelled,
    Success,
}
struct Fixture {
    name: &'static str,
    steps: Mutex<VecDeque<Step>>,
    calls: Arc<Mutex<Vec<String>>>,
}
#[async_trait]
impl Provider for Fixture {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        self.calls.lock().unwrap().push(self.name.into());
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected retry");
        let usage = Usage {
            input_tokens: if matches!(step, Step::Success) { 11 } else { 3 },
            output_tokens: 1,
            ..Default::default()
        };
        if matches!(step, Step::Success) {
            return Ok(ModelResponse {
                text: "done".into(),
                usage: Some(usage),
                ..Default::default()
            });
        }
        if matches!(step, Step::Cancelled) {
            cancellation.cancel();
        }
        let mut error = ProviderError::new(
            if matches!(step, Step::Auth) {
                ProviderErrorKind::Auth
            } else {
                ProviderErrorKind::Transient
            },
            if matches!(step, Step::Cancelled) {
                "cancelled"
            } else {
                "fixture failure"
            },
        );
        error.published = matches!(step, Step::Published);
        error.usage = Some(usage);
        Err(error.into())
    }
}
fn fixture(
    name: &'static str,
    steps: impl IntoIterator<Item = Step>,
    calls: &Arc<Mutex<Vec<String>>>,
) -> Arc<dyn Provider> {
    Arc::new(Fixture {
        name,
        steps: Mutex::new(steps.into_iter().collect()),
        calls: calls.clone(),
    })
}
fn request() -> ModelRequest {
    ModelRequest {
        model: "opencode-free/flash-free".into(),
        messages: vec![bubblepaw_core::Message {
            id: "source".into(),
            session_id: "fixture".into(),
            role: bubblepaw_core::Role::User,
            origin: None,
            content: "summarize".into(),
            tool_calls: vec![],
            tool_call_id: None,
            provider_state: None,
            is_error: false,
            created_at: "now".into(),
        }],
        ..Default::default()
    }
}
fn registry(
    paid: impl IntoIterator<Item = Step>,
    calls: &Arc<Mutex<Vec<String>>>,
) -> ProviderRegistry {
    let mut registry = ProviderRegistry::new("opencode-free", fixture("free", [Step::Auth], calls));
    registry
        .insert("opencode", fixture("go", paid, calls))
        .unwrap();
    registry
        .set_model_fallbacks(
            "opencode-free/flash-free".into(),
            vec!["opencode/flash".into()],
        )
        .unwrap();
    registry
}

#[tokio::test]
async fn auxiliary_retries_go_inside_fallback_once_and_keeps_success_sticky() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let registry = registry(
        [
            Step::Transient,
            Step::Success,
            Step::Transient,
            Step::Success,
        ],
        &calls,
    );
    let first = registry
        .generate_auxiliary(request(), CancellationToken::new(), false, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(calls.lock().unwrap().as_slice(), ["free", "go", "go"]);
    assert_eq!(first.usage.as_ref().unwrap().input_tokens, 17);
    assert!(registry.selected.lock().unwrap().is_empty());
    assert!(registry.last.lock().unwrap().is_empty());
    assert_eq!(
        registry
            .auxiliary_selected
            .lock()
            .unwrap()
            .get(&("fixture".into(), "opencode-free/flash-free".into())),
        Some(&"opencode/flash".to_owned())
    );
    assert_eq!(
        first.last_request_usage.as_ref().unwrap().input_tokens,
        Some(11)
    );
    assert_eq!(
        ProviderRegistry::selected_model(&first),
        Some("opencode/flash")
    );
    let second = registry
        .generate_auxiliary(request(), CancellationToken::new(), false, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        ["free", "go", "go", "go", "go"]
    );
    assert_eq!(second.usage.as_ref().unwrap().input_tokens, 14);
    assert!(registry.selected.lock().unwrap().is_empty());
    assert!(registry.last.lock().unwrap().is_empty());
}

#[tokio::test]
async fn auxiliary_go_exhaustion_is_four_attempts_and_preserves_every_usage() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let registry = registry([Step::Transient; 4], &calls);
    let error = registry
        .generate_auxiliary(request(), CancellationToken::new(), false, Duration::ZERO)
        .await
        .unwrap_err();
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        ["free", "go", "go", "go", "go"]
    );
    assert_eq!(
        ProviderError::of(&error)
            .unwrap()
            .usage
            .as_ref()
            .unwrap()
            .input_tokens,
        15
    );
}

#[tokio::test]
async fn auxiliary_publication_and_cancellation_do_not_retry_or_fall_through() {
    for step in [Step::Published, Step::Cancelled] {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let registry = registry([step], &calls);
        assert!(
            registry
                .generate_auxiliary(request(), CancellationToken::new(), false, Duration::ZERO)
                .await
                .is_err()
        );
        assert_eq!(calls.lock().unwrap().as_slice(), ["free", "go"]);
    }
}

#[tokio::test]
async fn auxiliary_and_main_routes_retry_only_unpublished_failures() {
    // Auxiliary calls share the registry's one policy: an unpublished transient
    // failure is retried, and a published one is not.
    let calls = Arc::new(Mutex::new(Vec::new()));
    let other = ProviderRegistry::new(
        "other",
        fixture("other", [Step::Transient, Step::Published], &calls),
    );
    let mut req = request();
    req.model = "other/model".into();
    assert!(
        other
            .generate_auxiliary(req, CancellationToken::new(), false, Duration::ZERO)
            .await
            .is_err()
    );
    assert_eq!(calls.lock().unwrap().as_slice(), ["other", "other"]);
    calls.lock().unwrap().clear();
    // The main turn retries the unpublished transient failure once, then stops
    // at the published one: free(auth) -> go(transient) -> go(published).
    let registry = registry([Step::Transient, Step::Published], &calls);
    assert!(
        registry
            .generate(request(), Arc::new(|_| {}), CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(calls.lock().unwrap().as_slice(), ["free", "go", "go"]);
}

struct IdentityFixture {
    fail: bool,
    seen: Arc<Mutex<Vec<String>>>,
}
#[async_trait]
impl Provider for IdentityFixture {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.seen.lock().unwrap().push(request.model);
        if self.fail {
            return Err(ProviderError::new(ProviderErrorKind::Auth, "free unavailable").into());
        }
        Ok(ModelResponse {
            text: "{\"safe\":true}".into(),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn llm_decisions_report_raw_selected_model_without_changing_routed_dispatch() {
    for fallback in [false, true] {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ProviderRegistry::new(
            "main",
            Arc::new(IdentityFixture {
                fail: fallback,
                seen: seen.clone(),
            }),
        );
        if fallback {
            registry
                .insert(
                    "opencode",
                    Arc::new(IdentityFixture {
                        fail: false,
                        seen: seen.clone(),
                    }),
                )
                .unwrap();
            registry
                .set_model_fallbacks(
                    "main/vendor/raw-model".into(),
                    vec!["opencode/vendor/fallback-model".into()],
                )
                .unwrap();
        }
        let response = registry
            .decide_with_reasoning(
                DecisionRequest {
                    model: "llm/main/vendor/raw-model".into(),
                    input: "synthetic".into(),
                    questions: vec![bubblepaw_core::Question {
                        name: "safe".into(),
                        instructions: "Safe?".into(),
                        kind: bubblepaw_core::QuestionKind::Predicate,
                    }],
                    session_id: Some("fixture".into()),
                    reasoning_effort: Default::default(),
                },
                CancellationToken::new(),
            )
            .await
            .unwrap()
            .0;
        let expected = if fallback {
            "vendor/fallback-model"
        } else {
            "vendor/raw-model"
        };
        assert_eq!(response.model, expected);
        assert_eq!(seen.lock().unwrap()[0], "vendor/raw-model");
        if fallback {
            assert_eq!(seen.lock().unwrap()[1], expected);
        }
    }
}
