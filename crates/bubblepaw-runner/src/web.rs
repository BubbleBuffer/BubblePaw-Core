//! Local, optional HTTP host. The in-memory SQLite view is disposable; the
//! engine's Store remains the only archive. Paragraphs never touch either DB.
//! Every route makes its host calls through `host::call`; this module keeps
//! only the live paragraphs of the running turn.
use crate::{
    artifacts::ArtifactStore,
    codemode::CodeMode,
    host_api::{self, views::ServingView},
    input_files::{InputFiles, InputLimits},
    tools::ProcessTools,
    wake::{WakeCoordinator, WakeDriver, WakeRegistration},
};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, rejection::JsonRejection},
    http::StatusCode,
    middleware,
    routing::{get, post},
};
use bubblepaw_core::{CancellationToken, EventKind, Harness, Store};
use errors::ApiError;
use events::HostEvent;
use serde::Serialize;
use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    path::PathBuf,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{Notify, broadcast},
};

#[path = "web_context.rs"]
mod context;
#[path = "web_question.rs"]
mod question_routes;
use question_routes::{answer_child_question, answer_question};

mod boundary;
mod errors;
mod events;
mod host;
mod rpc;
mod runs;
mod sessions;

pub use crate::host_api::views::{SessionSummary, SessionView, ViewLimits};

#[derive(Clone, Default)]
pub struct WebServices {
    /// Set by the CLI host, which owns shared runtime shutdown and request roots.
    pub session_scope: Option<crate::session_scope::SessionScope>,
    /// HTTP defaults to host-wide reads; embedders can restrict a caller.
    pub read_scope: Option<host_api::ReadScope>,
    pub storage_policy: Option<bubblepaw_core::RootStoragePolicy>,
    pub provider_routes: Option<Router>,
    pub provider_setup: Option<crate::provider_setup::ProviderSetup>,
    pub providers: Option<Arc<crate::provider::LiveProviderConfig>>,
    pub live_config: Option<Arc<crate::live_config::LiveConfig>>,
    pub agents: Option<Arc<crate::agents::AgentTools>>,
    pub repl: Option<Arc<CodeMode>>,
    pub artifacts: Option<Arc<ArtifactStore>>,
    pub input_files: Option<Arc<InputFiles>>,
    pub processes: Option<Arc<ProcessTools>>,
    pub questions: Option<Arc<crate::question::QuestionTools>>,
    pub context_strategy: bubblepaw_core::ContextStrategy,
    /// Resolved once from the launch configuration (`EffectiveConfig::codex_models_cache`).
    pub codex_models_cache: Option<PathBuf>,
}

const PARAGRAPH_LIMIT: usize = 256;
const LIVE_BYTES: usize = 512 * 1024;
const EVENT_CAPACITY: usize = 128;
const BODY_LIMIT: usize = 256 * 1024;

#[derive(Clone, Debug, Serialize)]
pub struct WebConfig {
    pub workspace: String,
    pub model: String,
    pub provider: String,
    /// Tool access policy; this does not prevent sending prompts.
    pub read_only: bool,
}

#[derive(Default)]
struct LiveRound {
    run_id: String,
    last_tool_call: Option<String>,
    paragraphs: VecDeque<String>,
    bytes: usize,
    truncated: bool,
}
#[derive(Default)]
struct Transient {
    sessions: HashMap<String, LiveRound>,
    sequence: Option<u64>,
    /// Live events per session processed since the last durable event.
    live: HashMap<String, u64>,
}
struct WebState {
    harness: Arc<Harness>,
    store: Store,
    config: WebConfig,
    port: u16,
    serving: Arc<Mutex<ServingView>>,
    transient: Mutex<Transient>,
    events: broadcast::Sender<HostEvent>,
    refresh: Arc<Notify>,
    stop: CancellationToken,
    services: WebServices,
    wake: Option<Arc<WakeCoordinator>>,
    wake_host: Mutex<Option<(WakeRegistration, WakeDriver)>>,
}
impl Drop for WebState {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// Build a host router on an existing Tokio runtime. `port` must be the actual
/// listener port (including the assigned port when binding to port zero).
pub fn router(harness: Arc<Harness>, store: Store, config: WebConfig, port: u16) -> Result<Router> {
    build_router(harness, store, config, port, WebServices::default()).map(|(router, _)| router)
}
pub fn router_with_services(
    harness: Arc<Harness>,
    store: Store,
    config: WebConfig,
    port: u16,
    services: WebServices,
) -> Result<Router> {
    build_router(harness, store, config, port, services).map(|(router, _)| router)
}

fn build_router(
    harness: Arc<Harness>,
    store: Store,
    config: WebConfig,
    port: u16,
    services: WebServices,
) -> Result<(Router, CancellationToken)> {
    let (events, _) = broadcast::channel(EVENT_CAPACITY);
    if let Some(questions) = &services.questions {
        questions.set_child_answer_harness(harness.clone())?;
    }
    let wake = services
        .agents
        .as_ref()
        .map(|agents| agents.wake_coordinator())
        .or_else(|| {
            services
                .processes
                .as_ref()
                .map(|processes| processes.wake_coordinator())
        });
    if let (Some(wake), Some(processes)) = (&wake, &services.processes) {
        processes
            .try_set_wake(wake.clone())
            .context("Process and agent tools must share the wake coordinator")?;
        ensure!(
            Arc::ptr_eq(&processes.wake_coordinator(), wake),
            "Process and agent tools must share the wake coordinator"
        );
        processes.set_store(store.clone());
    }
    let wake_host = wake.as_ref().map(|wake| {
        (
            wake.register_harness_scoped(
                harness.clone(),
                services
                    .session_scope
                    .clone()
                    .unwrap_or_else(crate::session_scope::SessionScope::all),
            ),
            wake.start_driver(),
        )
    });
    let state = Arc::new(WebState {
        harness,
        store,
        config,
        port,
        serving: Arc::new(Mutex::new(ServingView::new()?)),
        transient: Mutex::default(),
        events,
        refresh: Arc::new(Notify::new()),
        stop: CancellationToken::new(),
        services,
        wake,
        wake_host: Mutex::new(wake_host),
    });
    start_tasks(&state)?;
    let stop = state.stop.clone();
    let input_limits = state
        .services
        .input_files
        .as_ref()
        .map_or_else(InputLimits::default, |files| files.limits());
    let json_limit = BODY_LIMIT.max(
        input_limits
            .prompt_chars
            .saturating_mul(6)
            .saturating_add(16 * 1024),
    );
    let upload_limit = input_limits.max_file_bytes as usize;
    Ok((
        Router::new()
            .route("/api/config", get(sessions::get_config))
            .route(
                "/api/sessions",
                get(sessions::sessions).post(sessions::create_session),
            )
            .route("/api/sessions/{id}", get(sessions::session))
            .route("/api/sessions/{id}/rename", post(sessions::rename_session))
            .route("/api/sessions/{id}/messages", get(context::messages))
            .merge(context::routes())
            .route("/api/sessions/{id}/run", post(runs::run))
            .route("/api/sessions/{id}/resume", post(runs::resume))
            .route("/api/sessions/{id}/steer", post(runs::steer))
            .route("/api/sessions/{id}/followup", post(runs::followup))
            .route("/api/sessions/{id}/cancel", post(runs::cancel))
            .route(
                "/api/sessions/{id}/processes",
                get(sessions::list_processes),
            )
            .route(
                "/api/sessions/{id}/processes/{handle}/background",
                post(sessions::background_process),
            )
            .route("/api/sessions/{id}/fork", post(sessions::fork))
            .route("/api/sessions/{id}/rewind", post(sessions::rewind))
            .route("/api/sessions/{id}/approvals", get(sessions::approvals))
            .route("/api/approvals/{id}", post(sessions::decide_approval))
            .route("/api/sessions/{id}/question/answer", post(answer_question))
            .route(
                "/api/sessions/{id}/children/{child}/question/answer",
                post(answer_child_question),
            )
            .route(
                "/api/sessions/{id}/attachments",
                post(sessions::upload_attachment).layer(DefaultBodyLimit::max(upload_limit)),
            )
            .route(
                "/api/sessions/{id}/attachments/{file}",
                get(sessions::download_attachment),
            )
            .route(
                "/api/sessions/{id}/environments",
                get(sessions::environments).post(sessions::control_environment),
            )
            .route(
                "/api/sessions/{id}/artifacts/{artifact}",
                get(sessions::artifact),
            )
            .route("/api/events", get(events::events_stream))
            .route("/rpc", post(rpc::rpc))
            .merge(
                state
                    .services
                    .provider_routes
                    .clone()
                    .unwrap_or_default()
                    .with_state(()),
            )
            .fallback(root_notice)
            .layer(DefaultBodyLimit::max(json_limit))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                boundary::local_boundary,
            ))
            .with_state(state),
        stop,
    ))
}

/// The host serves no UI; any unmatched path answers with a pointer to `/rpc`.
async fn root_notice() -> (StatusCode, &'static str) {
    (
        StatusCode::NOT_FOUND,
        "BubblePaw host: JSON-RPC over HTTP at POST /rpc, events at GET /api/events.
",
    )
}

/// Serve only an already-bound loopback listener, draining admitted runs when
/// shutdown is requested. The host never opens a credential endpoint.
pub async fn serve_with_services(
    listener: TcpListener,
    harness: Arc<Harness>,
    store: Store,
    config: WebConfig,
    services: WebServices,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let address = listener.local_addr()?;
    let owns_runtime = services.session_scope.is_none();
    ensure!(
        address.ip().is_loopback(),
        "The web host must bind to loopback"
    );
    let (app, stop) = build_router(harness.clone(), store, config, address.port(), services)?;
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown.await;
            stop.cancel();
        })
        .await;
    if owns_runtime {
        harness.shutdown(Duration::from_secs(5)).await?;
    }
    result.context("Local HTTP server failed")
}

fn start_tasks(state: &Arc<WebState>) -> Result<()> {
    if state.wake.is_some() {
        let weak = Arc::downgrade(state);
        let stop = state.stop.clone();
        tokio::spawn(async move {
            stop.cancelled().await;
            if let Some(state) = weak.upgrade() {
                // Remove admission callbacks before draining the host's runs.
                state.wake_host.lock().unwrap().take();
            }
        });
    }
    // Subscribe before the views read, so no event falls between them.
    let subscription = host_api::events::subscribe(&host::host_ctx(state), None, None)?;
    let mut receiver = subscription.receiver;
    // Events up to here are already in the store the views read.
    state.transient.lock().unwrap().sequence = Some(subscription.last_seq);
    let weak = Arc::downgrade(state);
    let stop = state.stop.clone();
    tokio::spawn(async move {
        loop {
            let received =
                tokio::select! { _ = stop.cancelled() => break, value = receiver.recv() => value };
            let Some(state) = weak.upgrade() else {
                break;
            };
            match received {
                Ok(event) => {
                    let durable = event.durable;
                    let visible = host_api::events::visible(&host::host_ctx(&state), &event, None);
                    {
                        let mut transient = state.transient.lock().unwrap();
                        // Durable events are numbered one by one; a live event
                        // carries the last durable sequence (docs/session-api.md).
                        let next = if durable {
                            event.sequence.checked_sub(1)
                        } else {
                            Some(event.sequence)
                        };
                        let gap = transient
                            .sequence
                            .is_some_and(|previous| next.is_some_and(|next| next > previous));
                        if gap {
                            transient.sessions.clear();
                            state.refresh.notify_one();
                            let _ = state.events.send(HostEvent::Reset);
                        }
                        // An event the views already reflect must not rewind
                        // the cursor or restore an obsolete live round.
                        if transient
                            .sequence
                            .is_some_and(|previous| next.is_none_or(|next| next < previous))
                        {
                            continue;
                        }
                        transient.sequence = Some(event.sequence);
                        if durable {
                            transient.live.clear();
                        }
                        if !visible {
                            continue;
                        }
                        if !durable && matches!(event.kind, EventKind::Paragraph { .. }) {
                            // Progress deltas are not paragraphs and are not counted here.
                            *transient.live.entry(event.session_id.clone()).or_default() += 1;
                        }
                        match &event.kind {
                            EventKind::RunStarted { .. } => {
                                if transient.sessions.len() >= host_api::views::SESSION_LIMIT {
                                    transient.sessions.clear();
                                    let _ = state.events.send(HostEvent::Reset);
                                }
                                transient.sessions.insert(
                                    event.session_id.clone(),
                                    LiveRound {
                                        run_id: event.run_id.clone(),
                                        ..Default::default()
                                    },
                                );
                            }
                            EventKind::Paragraph { text } => {
                                if !transient.sessions.contains_key(&event.session_id)
                                    && transient.sessions.len() >= host_api::views::SESSION_LIMIT
                                {
                                    transient.sessions.clear();
                                    let _ = state.events.send(HostEvent::Reset);
                                }
                                let round = transient
                                    .sessions
                                    .entry(event.session_id.clone())
                                    .or_insert_with(|| LiveRound {
                                        run_id: event.run_id.clone(),
                                        truncated: true,
                                        ..Default::default()
                                    });
                                if round.run_id != event.run_id {
                                    *round = LiveRound {
                                        run_id: event.run_id.clone(),
                                        truncated: true,
                                        ..Default::default()
                                    };
                                }
                                let mut text = text.clone();
                                if text.len() > LIVE_BYTES {
                                    let mut cutoff = LIVE_BYTES;
                                    while !text.is_char_boundary(cutoff) {
                                        cutoff -= 1;
                                    }
                                    text.truncate(cutoff);
                                    round.truncated = true;
                                }
                                round.bytes += text.len();
                                round.paragraphs.push_back(text);
                                while round.paragraphs.len() > PARAGRAPH_LIMIT
                                    || round.bytes > LIVE_BYTES
                                {
                                    if let Some(removed) = round.paragraphs.pop_front() {
                                        round.bytes -= removed.len();
                                        round.truncated = true;
                                    }
                                }
                            }
                            EventKind::ToolStarted { call } => {
                                if let Some(round) = transient.sessions.get_mut(&event.session_id) {
                                    round.paragraphs.clear();
                                    round.bytes = 0;
                                    round.truncated = false;
                                    round.last_tool_call = Some(call.id.clone());
                                }
                            }
                            EventKind::RunFinished { .. } | EventKind::SessionSettled {} => {
                                transient.sessions.remove(&event.session_id);
                            }
                            _ => {}
                        }
                    }
                    if durable {
                        state.refresh.notify_one();
                    }
                    let _ = state.events.send(HostEvent::Core(Arc::new(event)));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    state.transient.lock().unwrap().sessions.clear();
                    state.refresh.notify_one();
                    let _ = state.events.send(HostEvent::Reset);
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    events::forward_mcp_status(state);
    let weak = Arc::downgrade(state);
    let refresh = state.refresh.clone();
    let stop = state.stop.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! { _ = stop.cancelled() => break, _ = refresh.notified() => {} }
            let weak = weak.clone();
            let _ = tokio::task::spawn_blocking(move || refresh_cached(weak)).await;
        }
    });
    Ok(())
}

/// Rebuilds the host's cached views after a durable change.
fn refresh_cached(weak: Weak<WebState>) -> Result<()> {
    let Some(state) = weak.upgrade() else {
        return Ok(());
    };
    host_api::views::refresh_cached(&host::host_ctx(&state))?;
    Ok(())
}

fn usage_settings(state: &WebState) -> crate::usage::UsageSettings {
    state
        .services
        .providers
        .as_ref()
        .map(|providers| crate::usage::UsageSettings {
            codex_models_cache: state.services.codex_models_cache.clone(),
            ..crate::usage::UsageSettings::from_settings(&providers.snapshot().settings)
        })
        .unwrap_or_else(|| crate::usage::UsageSettings {
            default_model: state.config.model.clone(),
            default_provider: state.config.provider.clone(),
            codex_models_cache: state.services.codex_models_cache.clone(),
            ..Default::default()
        })
}

fn body<T>(value: std::result::Result<Json<T>, JsonRejection>) -> std::result::Result<T, ApiError> {
    value.map(|Json(value)| value).map_err(|error| {
        use axum::http::StatusCode;
        let status = match error.status() {
            StatusCode::UNPROCESSABLE_ENTITY => StatusCode::BAD_REQUEST,
            status => status,
        };
        ApiError(status, error.body_text())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::{events::events_stream, sessions::session};
    use async_trait::async_trait;
    use axum::{
        extract::{Path, State},
        http::HeaderMap,
        response::IntoResponse,
    };
    use bubblepaw_core::{
        HarnessConfig, ModelRequest, ModelResponse, Provider, RunOptions, TextCallback, Tool,
        ToolCall, ToolContext, ToolDefinition, ToolRegistry, ToolResult,
    };
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Semaphore;

    struct Rounds(AtomicUsize);
    #[async_trait]
    impl Provider for Rounds {
        async fn generate(
            &self,
            _request: ModelRequest,
            on_text: TextCallback,
            _cancellation: CancellationToken,
        ) -> Result<ModelResponse> {
            let round = self.0.fetch_add(1, Ordering::Relaxed);
            let text = format!("Persisted round {round}.");
            on_text(&format!("{text}\n\n"));
            Ok(ModelResponse {
                text,
                tool_calls: vec![ToolCall {
                    id: format!("round-{round}"),
                    name: "gate".into(),
                    arguments: json!({}),
                }],
                provider_state: Some(json!({"private":"continuation-fixture"})),
                ..Default::default()
            })
        }
    }
    struct Gate(Semaphore);
    #[async_trait]
    impl Tool for Gate {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                approval: Default::default(),
                name: "gate".into(),
                description: "fixture".into(),
                input_schema: json!({"type":"object"}),
                read_only: true,
            }
        }
        async fn invoke(&self, _context: ToolContext, _input: Value) -> Result<ToolResult> {
            self.0.acquire().await?.forget();
            Ok(ToolResult::text("fixture"))
        }
    }

    #[tokio::test]
    async fn canonical_phase_suppresses_stale_paragraphs_and_live_gets_do_not_write_serving_sqlite()
    {
        let store = Store::in_memory().unwrap();
        let session_record = store.create_session("phase fixture").unwrap();
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(Gate(Semaphore::new(1)))).unwrap();
        let harness = Arc::new(Harness::new(
            store.clone(),
            Arc::new(Rounds(AtomicUsize::new(0))),
            tools,
            HarnessConfig::default(),
        ));
        let mut receiver = harness.subscribe();
        let run = harness
            .start(&session_record.id, "fixture", RunOptions::default())
            .unwrap();
        loop {
            let event = receiver.recv().await.unwrap();
            if matches!(event.kind, EventKind::ToolStarted { call } if call.id == "round-1") {
                break;
            }
        }
        let (events, _) = broadcast::channel(4);
        let state = Arc::new(WebState {
            harness: harness.clone(),
            store: store.clone(),
            config: WebConfig {
                workspace: "fixture".into(),
                model: "fixture".into(),
                provider: "demo".into(),
                read_only: true,
            },
            port: 8788,
            serving: Arc::new(Mutex::new(ServingView::new().unwrap())),
            transient: Mutex::default(),
            events,
            refresh: Arc::new(Notify::new()),
            stop: CancellationToken::new(),
            services: WebServices::default(),
            wake: None,
            wake_host: Mutex::new(None),
        });
        // Simulate the pump lagging behind the second persisted assistant:
        // its last processed tool boundary still belongs to the first round.
        state.transient.lock().unwrap().sessions.insert(
            session_record.id.clone(),
            LiveRound {
                run_id: run.id,
                last_tool_call: Some("round-0".into()),
                paragraphs: VecDeque::from(["Persisted round 1.".into()]),
                bytes: 18,
                truncated: false,
            },
        );
        let Json(view) = session(State(state.clone()), Path(session_record.id.clone()))
            .await
            .unwrap();
        assert!(view.live_paragraphs.is_empty());
        assert_eq!(view.messages.last().unwrap().content, "Persisted round 1.");
        {
            let serving = state.serving.lock().unwrap();
            let data: String = serving
                .db
                .query_row("SELECT data FROM views", [], |row| row.get(0))
                .unwrap();
            assert!(!data.contains("provider_state"));
            assert!(!data.contains("continuation-fixture"));
        }
        let serving_writes = state.serving.lock().unwrap().db.total_changes();
        let archive_writes = store.revision();
        {
            let mut transient = state.transient.lock().unwrap();
            let round = transient.sessions.get_mut(&session_record.id).unwrap();
            round.last_tool_call = Some("round-1".into());
            round.paragraphs = VecDeque::from(["A new transient paragraph.".into()]);
        }
        let Json(view) = session(State(state.clone()), Path(session_record.id.clone()))
            .await
            .unwrap();
        assert_eq!(view.live_paragraphs, vec!["A new transient paragraph."]);
        assert_eq!(
            serving_writes,
            state.serving.lock().unwrap().db.total_changes()
        );
        assert_eq!(archive_writes, store.revision());
        harness.cancel(&session_record.id);
        harness.wait_for_idle(&session_record.id).await.unwrap();
    }

    #[tokio::test]
    async fn subscriber_gap_emits_reset_without_blocking_the_sender() {
        let store = Store::in_memory().unwrap();
        let harness = Arc::new(Harness::new(
            store.clone(),
            Arc::new(crate::provider::ScriptedProvider::new("fixture")),
            ToolRegistry::new(),
            HarnessConfig::default(),
        ));
        let (events, _) = broadcast::channel(1);
        let state = Arc::new(WebState {
            harness,
            store,
            config: WebConfig {
                workspace: "fixture".into(),
                model: "fixture".into(),
                provider: "demo".into(),
                read_only: true,
            },
            port: 8788,
            serving: Arc::new(Mutex::new(ServingView::new().unwrap())),
            transient: Mutex::default(),
            events,
            refresh: Arc::new(Notify::new()),
            stop: CancellationToken::new(),
            services: WebServices::default(),
            wake: None,
            wake_host: Mutex::new(None),
        });
        let response = events_stream(State(state.clone()), HeaderMap::new())
            .await
            .into_response();
        for _ in 0..4 {
            let _ = state.events.send(HostEvent::Reset);
        }
        let mut stream = response.into_body().into_data_stream();
        use futures_util::StreamExt;
        let initial = stream.next().await.unwrap().unwrap();
        assert!(
            std::str::from_utf8(&initial)
                .unwrap()
                .contains("event: reset")
        );
        let gap = stream.next().await.unwrap().unwrap();
        assert!(std::str::from_utf8(&gap).unwrap().contains("event: reset"));
    }
}

#[cfg(test)]
#[path = "web_freshness_tests.rs"]
mod freshness_tests;
#[cfg(test)]
#[path = "web_processes_tests.rs"]
mod processes_tests;
