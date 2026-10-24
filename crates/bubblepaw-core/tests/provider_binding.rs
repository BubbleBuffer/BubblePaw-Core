use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, ContextPreparation, Harness, HarnessConfig, ModelRequest, ModelResponse,
    PrepareCall, PreparedContext, Provider, RunOptions, Store, TextCallback, ToolRegistry,
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

struct Node {
    name: &'static str,
    seen: Arc<Mutex<Vec<(&'static str, String)>>>,
}
struct Decorated {
    inner: Arc<dyn Provider>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}
#[async_trait]
impl Provider for Decorated {
    async fn generate(
        &self,
        request: ModelRequest,
        text: TextCallback,
        cancellation: CancellationToken,
    ) -> anyhow::Result<ModelResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.generate(request, text, cancellation).await
    }
}
#[async_trait]
impl Provider for Node {
    async fn generate(
        &self,
        request: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> anyhow::Result<ModelResponse> {
        self.seen.lock().unwrap().push((self.name, request.model));
        Ok(ModelResponse {
            text: "done".into(),
            ..Default::default()
        })
    }
}
struct SuspendedPreparation {
    first: AtomicBool,
    entered: Notify,
    release: Notify,
}
#[async_trait]
impl ContextPreparation for SuspendedPreparation {
    async fn prepare(&self, call: PrepareCall<'_>) -> anyhow::Result<PreparedContext> {
        let request = call.request;
        if self.first.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(PreparedContext {
            request,
            notice: None,
            usage: None,
        })
    }
}

#[tokio::test]
async fn model_and_client_remain_bound_across_async_context_preparation() -> anyhow::Result<()> {
    let store = Store::in_memory()?;
    let session = store.create_session("request binding")?;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let old: Arc<dyn Provider> = Arc::new(Node {
        name: "old",
        seen: seen.clone(),
    });
    let new: Arc<dyn Provider> = Arc::new(Node {
        name: "new",
        seen: seen.clone(),
    });
    let changed = Arc::new(AtomicBool::new(false));
    let context = Arc::new(SuspendedPreparation {
        first: AtomicBool::new(true),
        entered: Notify::new(),
        release: Notify::new(),
    });
    let resolver = {
        let old = old.clone();
        let new = new.clone();
        let changed = changed.clone();
        Arc::new(move || {
            if changed.load(Ordering::SeqCst) {
                ("new-model".into(), new.clone())
            } else {
                ("old-model".into(), old.clone())
            }
        })
    };
    let harness = Harness::new(
        store,
        old,
        ToolRegistry::new(),
        HarnessConfig {
            context: context.clone(),
            provider_resolver: Some(resolver),
            ..Default::default()
        },
    );
    let decorated_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let harness = harness.map_provider({
        let calls = decorated_calls.clone();
        move |inner| {
            Arc::new(Decorated {
                inner,
                calls: calls.clone(),
            })
        }
    });
    harness.start(&session.id, "first", RunOptions::default())?;
    tokio::time::timeout(Duration::from_secs(5), context.entered.notified()).await?;
    changed.store(true, Ordering::SeqCst);
    context.release.notify_one();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), harness.wait_for_idle(&session.id))
            .await??
            .status,
        "completed"
    );
    assert_eq!(*seen.lock().unwrap(), vec![("old", "old-model".into())]);
    assert_eq!(
        harness
            .prompt(&session.id, "next", RunOptions::default())
            .await?
            .status,
        "completed"
    );
    assert_eq!(
        harness
            .prompt(
                &session.id,
                "pin",
                RunOptions {
                    model: Some("pinned-model".into()),
                    ..Default::default()
                }
            )
            .await?
            .status,
        "completed"
    );
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            ("old", "old-model".into()),
            ("new", "new-model".into()),
            ("new", "pinned-model".into())
        ]
    );
    assert_eq!(decorated_calls.load(Ordering::SeqCst), 3);
    Ok(())
}
