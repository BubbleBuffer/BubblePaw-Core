//! Same-store parity. The stdio framing function and the real `POST /rpc` route
//! answer from one harness and one store, with no writes between the calls, and
//! both read under the same `ReadScope`. Every read-profile method is compared on
//! its exact reply, with no masking: both sides read the same rows, so any
//! difference is a real one. An HTTP error shows even when the keys agree, and
//! the seeded counts are checked as values. Explicit scope cases live in
//! `same_store_scope.rs`.
use super::peer::post;
use anyhow::Result;
use bubblepaw_core::{Harness, HarnessConfig, Message, Role, Store, ToolRegistry};
use bubblepaw_runner::{
    host_api::{self, HostCtx, METHODS, Profile, ReadScope, views::ServingView},
    provider::ScriptedProvider,
    web::{WebConfig, WebServices, serve_with_services},
};
use serde_json::{Value, json};
use std::{
    future::Future,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::oneshot, task::JoinHandle};

const TIMEOUT: Duration = Duration::from_secs(60);
pub const SEEDED_MESSAGES: usize = 3;

/// Reads that must return a result, not an error, so the comparison cannot pass
/// by agreeing on two errors.
const MUST_SUCCEED: &[&str] = &[
    "initialize",
    "storage.stats",
    "session.list",
    "session.sidebar",
    "session.view",
    "session.state",
    "session.history",
];

pub struct Frame {
    pub method: &'static str,
    pub params: Value,
    pub bytes: Vec<u8>,
}

/// One request as the framing path answered it. `/rpc` was already checked to
/// answer the same, so `reply` stands for both.
pub struct Exchanged {
    pub method: &'static str,
    pub params: Value,
    pub reply: Option<Value>,
}

/// A running web host over the shared store. `stop` ends it.
pub struct Web {
    pub base: String,
    stop: oneshot::Sender<()>,
    server: JoinHandle<Result<()>>,
}

impl Web {
    pub async fn stop(self) -> Result<()> {
        let _ = self.stop.send(());
        self.server.await?
    }
}

/// The parameters each read method gets: none, the given session, and the
/// extra fields a few reads need to succeed.
fn read_params(session: &str, method: &str) -> Vec<Value> {
    let mut params = vec![json!({}), json!({"session_id": session})];
    match method {
        "session.search" => params.push(json!({"session_id": session, "query": "parity"})),
        "events.after" => params.push(json!({"session_id": session, "after": 0})),
        "history.messages" => params.push(json!({"session_id": session, "limit": 10})),
        _ => {}
    }
    params
}

/// Every read-profile method, one frame per parameter set, each with its own id.
pub fn frames(session: &str) -> Result<Vec<Frame>> {
    let mut frames = Vec::new();
    for method in METHODS
        .iter()
        .filter(|method| method.profile == Profile::Read)
    {
        for params in read_params(session, method.name) {
            let id = frames.len() + 1;
            let bytes = serde_json::to_vec(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method.name,
                "params": params.clone(),
            }))?;
            frames.push(Frame {
                method: method.name,
                params,
                bytes,
            });
        }
    }
    Ok(frames)
}

/// The config the web host reports, so `initialize` and `config.get` agree.
fn web_config(root: &Path) -> WebConfig {
    WebConfig {
        workspace: root.display().to_string(),
        model: "scripted".into(),
        provider: "demo".into(),
        read_only: false,
    }
}

/// A stdio-style context over the shared store, set up as the web host sets its
/// own: the same config, a serving view cache of its own, and the given read scope.
pub fn direct_ctx(
    harness: Arc<Harness>,
    store: Store,
    root: &Path,
    read_scope: ReadScope,
) -> Result<HostCtx> {
    let mut ctx = HostCtx::new(harness, store, root);
    let config = web_config(root);
    ctx.usage.default_model = config.model.clone();
    ctx.usage.default_provider = config.provider.clone();
    ctx.config = serde_json::to_value(config)?;
    ctx.views = Some(Arc::new(Mutex::new(ServingView::new()?)));
    ctx.read_scope = read_scope;
    Ok(ctx)
}

pub fn new_harness(store: &Store) -> Arc<Harness> {
    Arc::new(Harness::new(
        store.clone(),
        Arc::new(ScriptedProvider::new("")),
        ToolRegistry::default(),
        HarnessConfig::default(),
    ))
}

/// A session bound to `workspace`, with `messages` messages written before either
/// side reads. Message text is `"{title} {index}"`.
pub fn seed_session(
    store: &Store,
    workspace: &Path,
    title: &str,
    messages: usize,
) -> Result<String> {
    let session = store.create_session_with_workspace(title, workspace)?;
    let messages = (0..messages)
        .map(|index| Message {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: session.id.clone(),
            role: Role::Assistant,
            origin: None,
            content: format!("{title} {index}"),
            tool_calls: Vec::new(),
            tool_call_id: None,
            provider_state: None,
            is_error: false,
            created_at: "2026-10-29T00:00:00Z".into(),
        })
        .collect::<Vec<_>>();
    store.append_batch(&messages)?;
    Ok(session.id)
}

/// The main seeded session, bound to the root workspace.
pub fn seed(store: &Store, root: &Path) -> Result<String> {
    seed_session(store, root, "parity same store", SEEDED_MESSAGES)
}

/// Starts the real `serve_with_services` route over `store`. `read_scope` of
/// `None` leaves the web host on its default, which is host-wide reads.
pub async fn spawn_web(
    harness: Arc<Harness>,
    store: Store,
    root: &Path,
    read_scope: Option<ReadScope>,
) -> Result<Web> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let (stop, receive) = oneshot::channel::<()>();
    let services = WebServices {
        read_scope,
        ..WebServices::default()
    };
    let server = tokio::spawn(serve_with_services(
        listener,
        harness,
        store,
        web_config(root),
        services,
        async {
            let _ = receive.await;
        },
    ));
    Ok(Web { base, stop, server })
}

async fn bounded<T>(label: &str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(TIMEOUT, future)
        .await
        .unwrap_or_else(|_| panic!("{label} timed out"))
}

/// Sends one frame down both paths and returns the framing reply once both agree
/// exactly, status included.
pub async fn exchange(
    ctx: &HostCtx,
    client: &reqwest::Client,
    base: &str,
    frame: &Frame,
) -> Option<Value> {
    let label = format!("{} {}", frame.method, frame.params);
    let direct = bounded(
        frame.method,
        host_api::rpc::handle_bytes(ctx, Profile::Full, &frame.bytes),
    )
    .await;
    let (status, http) = bounded(frame.method, post(client, base, &frame.bytes)).await;
    assert_eq!(status.as_u16(), 200, "{label}: /rpc returned an HTTP error");
    assert!(direct.is_some(), "{label}: stdio framing gave no reply");
    assert_eq!(http, direct, "{label}: /rpc and stdio framing differ");
    direct
}

/// Every frame, in order, through `exchange`.
pub async fn exchange_all(
    ctx: &HostCtx,
    client: &reqwest::Client,
    base: &str,
    frames: Vec<Frame>,
) -> Vec<Exchanged> {
    let mut replies = Vec::new();
    for frame in frames {
        let reply = exchange(ctx, client, base, &frame).await;
        replies.push(Exchanged {
            method: frame.method,
            params: frame.params,
            reply,
        });
    }
    replies
}

/// The first successful result of `method`, if any.
pub fn result_of(replies: &[Exchanged], method: &str) -> Option<Value> {
    replies
        .iter()
        .filter(|exchanged| exchanged.method == method)
        .find_map(|exchanged| exchanged.reply.as_ref()?.get("result").cloned())
}

/// The reply to `method` with exactly `params`. A missing exchange is a fixture bug.
pub fn reply<'a>(replies: &'a [Exchanged], method: &str, params: Value) -> &'a Value {
    replies
        .iter()
        .find(|exchanged| exchanged.method == method && exchanged.params == params)
        .and_then(|exchanged| exchanged.reply.as_ref())
        .unwrap_or_else(|| panic!("no {method} exchange with {params}"))
}

/// The must-succeed reads return results, and the seeded session counts its messages.
pub fn assert_seeded(replies: &[Exchanged]) {
    for name in MUST_SUCCEED {
        assert!(
            result_of(replies, name).is_some(),
            "{name} never returned a result on either side"
        );
    }
    let view = result_of(replies, "session.view").expect("session.view result");
    assert_eq!(
        view["view"]["session"]["message_count"],
        json!(SEEDED_MESSAGES),
        "the session view does not count the seeded messages"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_store_replies_are_identical_for_every_read_method() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = Store::open_data_dir(root.path().join("data"))?;
    let session = seed(&store, root.path())?;
    let harness = new_harness(&store);
    let ctx = direct_ctx(harness.clone(), store.clone(), root.path(), ReadScope::Host)?;
    // No web read scope is set here, so `POST /rpc` reads host-wide by default.
    let web = spawn_web(harness, store, root.path(), None).await?;
    let client = reqwest::Client::new();
    let replies = exchange_all(&ctx, &client, &web.base, frames(&session)?).await;
    web.stop().await?;
    assert_seeded(&replies);
    Ok(())
}
