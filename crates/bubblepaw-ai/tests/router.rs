//! The router with no host: fallback to the next route after a failure that is
//! not a quota stop, and the sticky selection written only after a success.
use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_ai::{
    CancellationToken, ModelRequest, ModelResponse, NoObserver, Provider, ProviderError,
    ProviderErrorKind, Router, StreamCallback, adapter::RetryPolicy,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

struct Scripted {
    calls: AtomicUsize,
    ok: AtomicBool,
}
impl Scripted {
    fn new(ok: bool) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            ok: AtomicBool::new(ok),
        })
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}
#[async_trait]
impl Provider for Scripted {
    async fn generate(
        &self,
        _: ModelRequest,
        _: StreamCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.ok.load(Ordering::SeqCst) {
            return Ok(ModelResponse {
                text: "ok".into(),
                ..Default::default()
            });
        }
        Err(ProviderError::new(ProviderErrorKind::Fatal, "down").into())
    }
}

async fn call(router: &Router) -> Result<ModelResponse> {
    let request = ModelRequest {
        model: "a/m".into(),
        session_id: "s".into(),
        ..Default::default()
    };
    router
        .generate_routed(
            request,
            Arc::new(|_| {}),
            CancellationToken::new(),
            false,
            None,
            &NoObserver,
        )
        .await
}

#[tokio::test]
async fn falls_back_after_a_failure_and_selects_only_after_success() -> Result<()> {
    let (a, b) = (Scripted::new(false), Scripted::new(false));
    let mut router = Router::new("a", a.clone());
    router.insert("b", b.clone())?;
    router.set_retry_policy(RetryPolicy::once());
    router.set_model_fallbacks("a/m".into(), vec!["b/m".into()])?;

    // Every route fails: the error surfaces and nothing becomes sticky.
    assert!(call(&router).await.is_err());
    assert_eq!((a.calls(), b.calls()), (1, 1));
    assert!(router.selections().lock().unwrap().is_empty());

    // The next route serves it, is named in the reply and becomes sticky.
    b.ok.store(true, Ordering::SeqCst);
    let reply = call(&router).await?;
    let served = reply.route.expect("route stamped");
    assert_eq!(served.selected, "b/m");
    assert_eq!(served.fallback_from, vec!["a/m"]);
    assert_eq!(
        router
            .selections()
            .lock()
            .unwrap()
            .get(&("s".into(), "a/m".into())),
        Some(&"b/m".to_owned())
    );

    // The sticky route starts the next call: the failed primary is not retried.
    call(&router).await?;
    assert_eq!((a.calls(), b.calls()), (2, 3));
    Ok(())
}
