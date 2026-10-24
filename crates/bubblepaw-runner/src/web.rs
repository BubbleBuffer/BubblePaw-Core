//! Local, optional HTTP host. The in-memory SQLite view is disposable; the
//! engine's Store remains the only archive. Paragraphs never touch either DB.
use crate::{
    artifacts::ArtifactStore,
    codemode::CodeMode,
    input_files::{InputFileRef, InputFiles, InputLimits},
    session_control::{SessionControl, SlashCommand},
    tools::ProcessTools,
    wake::{WakeCoordinator, WakeDriver, WakeRegistration},
};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{
        DefaultBodyLimit, Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderMap, Method, StatusCode, Uri},
    middleware::{self, Next},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{get, post},
};
use bubblepaw_core::{
    ApprovalRecord, CancellationToken, ClientInfo, Decision, EventKind, Harness, HarnessEvent,
    Message, MessageOrigin, QueuedInput, Run, RunOptions, Session, Store, client_param,
};
use errors::{ApiError, ChildSessionRefusal};
use futures_util::stream;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    convert::Infallible,
    future::Future,
    net::IpAddr,
    path::PathBuf,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{Notify, broadcast},
};
use tower_http::services::{ServeDir, ServeFile};

mod errors;
mod store;
use store::{blocking, known_session, replayed_after};

#[derive(Clone, Default)]
pub struct WebServices {
    /// Set by the CLI host, which owns shared runtime shutdown and request roots.
    pub session_scope: Option<crate::session_scope::SessionScope>,
    pub storage_policy: Option<bubblepaw_core::RootStoragePolicy>,
    pub provider_routes: Option<Router>,
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

const SESSION_LIMIT: usize = 64;
const VIEW_LIMIT: usize = 4;
const VIEW_BYTES: usize = 16 * 1024 * 1024;
const MESSAGE_LIMIT: usize = 200;
const RUN_LIMIT: usize = 64;
const INPUT_LIMIT: usize = 128;
const PARAGRAPH_LIMIT: usize = 256;
const LIVE_BYTES: usize = 512 * 1024;
const EVENT_CAPACITY: usize = 128;
const BODY_LIMIT: usize = 256 * 1024;
/// SSE resume replays at most this many logged events; further behind gets `reset`.
const REPLAY_LIMIT: usize = 10 * bubblepaw_core::EVENTS_AFTER_LIMIT;

#[derive(Clone, Debug, Serialize)]
pub struct WebConfig {
    pub workspace: String,
    pub model: String,
    pub provider: String,
    /// Tool access policy; this does not prevent sending prompts.
    pub read_only: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub status: String,
    pub message_count: u64,
    pub last_message_preview: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ViewLimits {
    pub message_limit: usize,
    pub run_limit: usize,
    pub pending_input_limit: usize,
    pub messages_truncated: bool,
    pub runs_truncated: bool,
    pub pending_inputs_truncated: bool,
    pub live_paragraphs_truncated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionView {
    #[serde(default)]
    pub usage: Option<crate::usage::SessionUsage>,
    #[serde(default)]
    pub todo: Vec<crate::todo::TodoItem>,
    #[serde(default)]
    pub question: Option<Value>,
    pub session: SessionSummary,
    pub messages: Vec<Message>,
    /// JSON run metadata deliberately omits Run.text.
    pub runs: Vec<Value>,
    pub pending_inputs: Vec<QueuedInput>,
    pub active_run_id: Option<String>,
    pub live_paragraphs: Vec<String>,
    pub event_sequence: Option<u64>,
    /// The last durable event this view reflects; follow with events after it
    /// (docs/session-api.md).
    #[serde(default)]
    pub seq: u64,
    /// Live events of this session with sequence `seq` already reflected in
    /// `live_paragraphs`; a follower skips that many of them.
    #[serde(default)]
    pub live_seen: u64,
    pub limits: ViewLimits,
    #[serde(default)]
    pub outputs: Vec<bubblepaw_core::OutputRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repl: Option<Value>,
    /// Tool calls waiting for a decision (docs/approvals.md).
    #[serde(default)]
    pub approvals: Vec<ApprovalRecord>,
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
#[derive(Clone)]
enum HostEvent {
    Core(Arc<HarnessEvent>),
    Reset,
}
struct ServingView {
    db: Connection,
    list_revision: Option<u64>,
}
impl ServingView {
    fn new() -> Result<Self> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE sidebar(id TEXT PRIMARY KEY, data TEXT NOT NULL); CREATE TABLE views(id TEXT PRIMARY KEY, revision INTEGER NOT NULL, data TEXT NOT NULL);")?;
        Ok(Self {
            db,
            list_revision: None,
        })
    }
}
struct WebState {
    harness: Arc<Harness>,
    store: Store,
    config: WebConfig,
    port: u16,
    serving: Mutex<ServingView>,
    transient: Mutex<Transient>,
    events: broadcast::Sender<HostEvent>,
    refresh: Arc<Notify>,
    stop: CancellationToken,
    services: WebServices,
    control: SessionControl,
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
pub fn router(
    harness: Arc<Harness>,
    store: Store,
    config: WebConfig,
    assets: PathBuf,
    port: u16,
) -> Result<Router> {
    build_router(harness, store, config, assets, port, WebServices::default())
        .map(|(router, _)| router)
}
pub fn router_with_services(
    harness: Arc<Harness>,
    store: Store,
    config: WebConfig,
    assets: PathBuf,
    port: u16,
    services: WebServices,
) -> Result<Router> {
    build_router(harness, store, config, assets, port, services).map(|(router, _)| router)
}

fn build_router(
    harness: Arc<Harness>,
    store: Store,
    config: WebConfig,
    assets: PathBuf,
    port: u16,
    services: WebServices,
) -> Result<(Router, CancellationToken)> {
    ensure!(
        assets.join("index.html").is_file(),
        "Built frontend is missing: {}. Build the web frontend and pass its dist directory with --assets.",
        assets.join("index.html").display()
    );
    let assets = assets
        .canonicalize()
        .context("Cannot open frontend assets directory")?;
    let (events, _) = broadcast::channel(EVENT_CAPACITY);
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
    let control = SessionControl::new(
        harness.clone(),
        store.clone(),
        PathBuf::from(&config.workspace),
    )
    .with_live_config(services.live_config.clone())
    .with_questions(services.questions.clone())
    .with_agents(services.agents.clone())
    .with_wake(wake.clone());
    let state = Arc::new(WebState {
        harness,
        store,
        config,
        port,
        serving: Mutex::new(ServingView::new()?),
        transient: Mutex::default(),
        events,
        refresh: Arc::new(Notify::new()),
        stop: CancellationToken::new(),
        services,
        control,
        wake,
        wake_host: Mutex::new(wake_host),
    });
    start_tasks(&state);
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
            .route("/api/config", get(get_config))
            .route("/api/sessions", get(sessions).post(create_session))
            .route("/api/sessions/{id}", get(session))
            .route("/api/sessions/{id}/rename", post(rename_session))
            .route("/api/sessions/{id}/messages", get(messages))
            .route("/api/sessions/{id}/run", post(run))
            .route("/api/sessions/{id}/resume", post(resume))
            .route("/api/sessions/{id}/steer", post(steer))
            .route("/api/sessions/{id}/followup", post(followup))
            .route("/api/sessions/{id}/cancel", post(cancel))
            .route("/api/sessions/{id}/processes", get(list_processes))
            .route(
                "/api/sessions/{id}/processes/{handle}/background",
                post(background_process),
            )
            .route("/api/sessions/{id}/fork", post(fork))
            .route("/api/sessions/{id}/rewind", post(rewind))
            .route("/api/sessions/{id}/approvals", get(approvals))
            .route("/api/approvals/{id}", post(decide_approval))
            .route("/api/sessions/{id}/question/answer", post(answer_question))
            .route(
                "/api/sessions/{id}/attachments",
                post(upload_attachment).layer(DefaultBodyLimit::max(upload_limit)),
            )
            .route(
                "/api/sessions/{id}/attachments/{file}",
                get(download_attachment),
            )
            .route(
                "/api/sessions/{id}/environments",
                get(environments).post(control_environment),
            )
            .route("/api/sessions/{id}/artifacts/{artifact}", get(artifact))
            .route("/api/events", get(events_stream))
            .merge(
                state
                    .services
                    .provider_routes
                    .clone()
                    .unwrap_or_default()
                    .with_state(()),
            )
            .fallback_service(
                ServeDir::new(&assets).fallback(ServeFile::new(assets.join("index.html"))),
            )
            .layer(DefaultBodyLimit::max(json_limit))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                local_boundary,
            ))
            .with_state(state),
        stop,
    ))
}

/// Serve only an already-bound loopback listener, draining admitted runs when
/// shutdown is requested. The host never opens a credential endpoint.
pub async fn serve_with_services(
    listener: TcpListener,
    harness: Arc<Harness>,
    store: Store,
    config: WebConfig,
    assets: PathBuf,
    services: WebServices,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let address = listener.local_addr()?;
    let owns_runtime = services.session_scope.is_none();
    ensure!(
        address.ip().is_loopback(),
        "The web host must bind to loopback"
    );
    let (app, stop) = build_router(
        harness.clone(),
        store,
        config,
        assets,
        address.port(),
        services,
    )?;
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

fn start_tasks(state: &Arc<WebState>) {
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
    let mut receiver = state.harness.subscribe();
    // Events up to here are already in the store the views read.
    state.transient.lock().unwrap().sequence = Some(state.harness.last_sequence());
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
                    let visible = state
                        .store
                        .session(&event.session_id)
                        .is_ok_and(|session| !session.hidden);
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
                                if transient.sessions.len() >= SESSION_LIMIT {
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
                                    && transient.sessions.len() >= SESSION_LIMIT
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
}

fn summary(
    session: &Session,
    count: u64,
    last: Option<&Message>,
    run: Option<&Run>,
) -> SessionSummary {
    let mut updated = session.updated_at.clone();
    if let Some(message) = last {
        updated = updated.max(message.created_at.clone());
    }
    if let Some(run) = run {
        updated = updated.max(run.ended_at.as_ref().unwrap_or(&run.started_at).clone());
    }
    SessionSummary {
        id: session.id.clone(),
        title: session.title.clone(),
        created_at: session.created_at.clone(),
        updated_at: updated,
        status: run
            .map(|run| run.status.clone())
            .unwrap_or_else(|| "idle".into()),
        message_count: count,
        last_message_preview: last
            .map(|message| message.content.chars().take(160).collect())
            .unwrap_or_default(),
    }
}
fn metadata(run: &Run) -> Result<Value> {
    let mut value = serde_json::to_value(run)?;
    value.as_object_mut().unwrap().remove("text");
    Ok(value)
}

fn refresh_list(state: &WebState, serving: &mut ServingView) -> Result<Vec<SessionSummary>> {
    if serving.list_revision != Some(state.store.revision()) {
        let before = state.store.revision();
        let records = state.store.visible_session_summaries(SESSION_LIMIT)?;
        let mut summaries: Vec<_> = records
            .iter()
            .map(|record| {
                summary(
                    &record.session,
                    record.message_count,
                    record.last_message.as_ref(),
                    record.latest_run.as_ref(),
                )
            })
            .collect();
        for item in &mut summaries {
            if state.store.active_session_metadata(&item.id)?.is_some()
                && state
                    .store
                    .host_state(&item.id, "question")?
                    .is_some_and(|question| question["status"] == "pending")
            {
                item.status = "waiting_for_answer".into();
            }
        }
        summaries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        let tx = serving.db.transaction()?;
        tx.execute("DELETE FROM sidebar", [])?;
        for item in summaries {
            tx.execute(
                "INSERT INTO sidebar VALUES(?1,?2)",
                params![item.id, serde_json::to_string(&item)?],
            )?;
        }
        tx.commit()?;
        serving.list_revision = Some(before);
    }
    let mut statement = serving
        .db
        .prepare("SELECT data FROM sidebar ORDER BY json_extract(data,'$.updated_at') DESC")?;
    statement
        .query_map([], |row| row.get::<_, String>(0))?
        .map(|row| Ok(serde_json::from_str(&row?)?))
        .collect()
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
fn refresh_session(
    state: &WebState,
    serving: &mut ServingView,
    id: &str,
) -> Result<(SessionView, u64)> {
    let revision = state.store.revision();
    let cached: Option<(u64, String)> = serving
        .db
        .query_row("SELECT revision,data FROM views WHERE id=?1", [id], |row| {
            Ok((row.get::<_, i64>(0)? as u64, row.get(1)?))
        })
        .optional()?;
    if let Some((version, data)) = cached.filter(|(version, _)| *version == revision) {
        return Ok((serde_json::from_str(&data)?, version));
    }
    let snapshot = state
        .store
        .snapshot(id, MESSAGE_LIMIT, RUN_LIMIT, INPUT_LIMIT)?;
    let mut messages = snapshot.messages;
    for message in &mut messages {
        message.provider_state = None;
    }
    let mut view = SessionView {
        usage: Some(crate::usage::session_usage(
            &state.store,
            id,
            &usage_settings(state),
        )?),
        todo: crate::todo::list(&state.store, id)?,
        question: state
            .store
            .host_state(id, "question")?
            .filter(|value| value["status"] == "pending"),
        approvals: Vec::new(),
        session: summary(
            &snapshot.session,
            snapshot.message_count,
            messages.last(),
            snapshot.runs.last(),
        ),
        active_run_id: snapshot
            .runs
            .iter()
            .rev()
            .find(|run| run.status == "running")
            .map(|run| run.id.clone()),
        runs: snapshot.runs.iter().map(metadata).collect::<Result<_>>()?,
        limits: ViewLimits {
            message_limit: MESSAGE_LIMIT,
            run_limit: RUN_LIMIT,
            pending_input_limit: INPUT_LIMIT,
            messages_truncated: snapshot.message_count > messages.len() as u64,
            runs_truncated: snapshot.run_count > snapshot.runs.len() as u64,
            pending_inputs_truncated: snapshot.pending_input_count
                > snapshot.pending_inputs.len() as u64,
            live_paragraphs_truncated: false,
        },
        messages,
        pending_inputs: snapshot.pending_inputs,
        live_paragraphs: Vec::new(),
        event_sequence: None,
        seq: 0,
        live_seen: 0,
        outputs: state.store.outputs(id, 128)?,
        repl: None,
    };
    if view.question.is_some() {
        view.session.status = "waiting_for_answer".into();
    }
    let data = serde_json::to_string(&view)?;
    serving.db.execute("DELETE FROM views WHERE id=?1", [id])?;
    if data.len() <= VIEW_BYTES {
        serving.db.execute(
            "INSERT INTO views VALUES(?1,?2,?3)",
            params![id, snapshot.revision as i64, data],
        )?;
        serving.db.execute("DELETE FROM views WHERE rowid NOT IN (SELECT rowid FROM views ORDER BY rowid DESC LIMIT ?1)", [VIEW_LIMIT as i64])?;
        while serving.db.query_row(
            "SELECT COALESCE(SUM(length(CAST(data AS BLOB))),0) FROM views",
            [],
            |row| row.get::<_, i64>(0),
        )? > VIEW_BYTES as i64
        {
            serving.db.execute(
                "DELETE FROM views WHERE rowid=(SELECT MIN(rowid) FROM views)",
                [],
            )?;
        }
    }
    Ok((view, snapshot.revision))
}
fn refresh_cached(weak: Weak<WebState>) -> Result<()> {
    let Some(state) = weak.upgrade() else {
        return Ok(());
    };
    let mut serving = state.serving.lock().unwrap();
    refresh_list(&state, &mut serving)?;
    let ids: Vec<String> = {
        let mut statement = serving.db.prepare("SELECT id FROM views")?;
        statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    for id in ids {
        refresh_session(&state, &mut serving, &id)?;
    }
    Ok(())
}

fn body<T>(value: std::result::Result<Json<T>, JsonRejection>) -> std::result::Result<T, ApiError> {
    value
        .map(|Json(value)| value)
        .map_err(|error| ApiError(error.status(), error.body_text()))
}
async fn prepare_input(
    state: Arc<WebState>,
    id: String,
    text: String,
    attachments: Vec<String>,
) -> std::result::Result<String, ApiError> {
    known_session(&state, &id).await?;
    let Some(files) = state.services.input_files.clone() else {
        if !attachments.is_empty() {
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "Input attachments are unavailable".into(),
            ));
        }
        InputLimits::default()
            .validate_prompt(&text)
            .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))?;
        return Ok(text);
    };
    tokio::task::spawn_blocking(move || files.prepare(&id, &text, &attachments))
        .await
        .map_err(|error| ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?
        .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))
}
fn title(value: Option<String>) -> std::result::Result<String, ApiError> {
    crate::sessions::title(value.as_deref(), "New session")
        .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))
}

async fn get_config(State(state): State<Arc<WebState>>) -> Json<Value> {
    let mut value = serde_json::to_value(&state.config).unwrap();
    if let Some(providers) = &state.services.providers {
        let main = providers.main_model();
        if let Some((provider, _)) = main.split_once('/') {
            value["provider"] = json!(provider);
        }
        value["model"] = json!(main);
    }
    value["session_limit"] = json!(SESSION_LIMIT);
    value["input_limits"] = json!(
        state
            .services
            .input_files
            .as_ref()
            .map_or_else(InputLimits::default, |files| files.limits())
    );
    Json(value)
}
#[derive(Deserialize)]
struct UploadQuery {
    name: String,
}
async fn upload_attachment(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    Query(query): Query<UploadQuery>,
    request: axum::extract::Request,
) -> std::result::Result<Json<InputFileRef>, ApiError> {
    known_session(&state, &id).await?;
    let files = state.services.input_files.clone().ok_or_else(|| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "Input attachments are unavailable".into(),
        )
    })?;
    let limit = files.limits().max_file_bytes as usize;
    if request
        .headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|size| size > limit as u64)
    {
        return Err(ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Attachment exceeds the configured file limit".into(),
        ));
    }
    let bytes = to_bytes(request.into_body(), limit)
        .await
        .map_err(|error| ApiError(StatusCode::PAYLOAD_TOO_LARGE, error.to_string()))?;
    tokio::task::spawn_blocking(move || files.upload(&id, &query.name, &bytes))
        .await
        .map_err(|error| ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?
        .map(Json)
        .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))
}
async fn download_attachment(
    State(state): State<Arc<WebState>>,
    Path((id, file)): Path<(String, String)>,
) -> std::result::Result<Response, ApiError> {
    known_session(&state, &id).await?;
    let files = state.services.input_files.clone().ok_or_else(|| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "Input attachments are unavailable".into(),
        )
    })?;
    let (reference, bytes) = tokio::task::spawn_blocking(move || files.read(&id, &file))
        .await
        .map_err(|error| ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?
        .map_err(|error| ApiError(StatusCode::NOT_FOUND, error.to_string()))?;
    let ascii: String = reference
        .name
        .chars()
        .map(|c| if c.is_ascii() { c } else { '_' })
        .collect();
    let encoded: String = reference
        .name
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect();
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/octet-stream")
        .header(
            "content-disposition",
            format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}"),
        )
        .header("content-length", bytes.len())
        .header("x-content-type-options", "nosniff")
        .header("content-security-policy", "default-src 'none'; sandbox")
        .body(Body::from(bytes))
        .map_err(|error| ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))
}
async fn environments(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
) -> std::result::Result<Json<Value>, ApiError> {
    blocking(move || {
        state.store.session(&id)?;
        Ok(state
            .services
            .repl
            .as_ref()
            .map(|r| r.status(&id))
            .unwrap_or_else(|| json!({"environments":[]})))
    })
    .await
    .map(Json)
}
async fn control_environment(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    input: std::result::Result<Json<Value>, JsonRejection>,
) -> std::result::Result<Json<Value>, ApiError> {
    let input = body(input)?;
    let reply = blocking({
        let state = state.clone();
        move || {
            state.store.session(&id)?;
            state
                .services
                .repl
                .as_ref()
                .context("REPL is unavailable")?
                .control(&id, input)
        }
    })
    .await?;
    state.refresh.notify_one();
    let _ = state.events.send(HostEvent::Reset);
    Ok(Json(reply))
}
async fn artifact(
    State(state): State<Arc<WebState>>,
    Path((session, id)): Path<(String, String)>,
) -> std::result::Result<Response, ApiError> {
    let (meta, bytes) = blocking(move || {
        state.store.session(&session)?;
        state
            .services
            .artifacts
            .as_ref()
            .context("Artifacts unavailable")?
            .read(&session, &id)
    })
    .await
    .map_err(|error| ApiError(StatusCode::NOT_FOUND, error.1))?;
    let name: String = meta
        .name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let disposition = if meta.mime.starts_with("image/") {
        format!("inline; filename=\"{name}\"")
    } else {
        format!("attachment; filename=\"{name}\"")
    };
    let mut response = bytes.into_response();
    let headers = response.headers_mut();
    headers.insert(
        "content-type",
        meta.mime.parse().map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Invalid artifact media type".into(),
            )
        })?,
    );
    headers.insert(
        "content-disposition",
        disposition.parse().map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Invalid artifact name".into(),
            )
        })?,
    );
    headers.insert(
        "x-content-type-options",
        axum::http::HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        "cache-control",
        axum::http::HeaderValue::from_static("no-store"),
    );
    Ok(response)
}
async fn sessions(
    State(state): State<Arc<WebState>>,
) -> std::result::Result<Json<Vec<SessionSummary>>, ApiError> {
    blocking(move || {
        let mut serving = state.serving.lock().unwrap();
        refresh_list(&state, &mut serving)
    })
    .await
    .map(Json)
}
async fn session(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
) -> std::result::Result<Json<SessionView>, ApiError> {
    blocking(move || {
        let mut serving = state.serving.lock().unwrap();
        loop {
            let (mut view, revision) = refresh_session(&state, &mut serving, &id)?;
            let transient = state.transient.lock().unwrap();
            if state.store.revision() != revision {
                drop(transient);
                continue;
            }
            view.event_sequence = transient.sequence;
            view.seq = transient.sequence.unwrap_or_default();
            view.live_seen = transient.live.get(&id).copied().unwrap_or_default();
            view.repl = state.services.repl.as_ref().map(|repl| repl.status(&id));
            view.approvals = state.harness.pending_approvals(&id)?;
            if let Some(round) = transient
                .sessions
                .get(&id)
                .filter(|round| Some(&round.run_id) == view.active_run_id.as_ref())
            {
                let started = view
                    .runs
                    .iter()
                    .find(|run| run["id"].as_str() == Some(&round.run_id))
                    .and_then(|run| run["started_at"].as_str())
                    .unwrap_or("");
                let persisted = view.messages.iter().rev().find(|message| {
                    message.role == bubblepaw_core::Role::Assistant
                        && message.created_at.as_str() >= started
                });
                let phase_is_current = persisted.is_none_or(|message| {
                    round
                        .last_tool_call
                        .as_ref()
                        .is_some_and(|id| message.tool_calls.iter().any(|call| &call.id == id))
                });
                if phase_is_current {
                    view.live_paragraphs = round.paragraphs.iter().cloned().collect();
                }
                view.limits.live_paragraphs_truncated = round.truncated;
            }
            return Ok(view);
        }
    })
    .await
    .map(Json)
}
#[derive(Deserialize)]
struct MessagesQuery {
    before: Option<String>,
    limit: Option<usize>,
}
#[derive(Serialize)]
struct MessagesPage {
    messages: Vec<Message>,
    has_more: bool,
}
async fn messages(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    query: std::result::Result<Query<MessagesQuery>, QueryRejection>,
) -> std::result::Result<Json<MessagesPage>, ApiError> {
    let Query(query) =
        query.map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.body_text()))?;
    let limit = query.limit.unwrap_or(100);
    if !(1..=MESSAGE_LIMIT).contains(&limit) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("Limit must be between 1 and {MESSAGE_LIMIT}"),
        ));
    }
    let rows = blocking(move || {
        state.store.session(&id)?;
        state
            .store
            .messages_before(&id, query.before.as_deref(), limit + 1)
    })
    .await?
    .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "Unknown before message id".into()))?;
    let has_more = rows.len() > limit;
    // The read is ascending, so discard the oldest extra row for continuation.
    let messages = rows
        .into_iter()
        .skip(usize::from(has_more))
        .map(|(_, mut message)| {
            message.provider_state = None;
            message
        })
        .collect();
    Ok(Json(MessagesPage { messages, has_more }))
}
#[derive(Default, Deserialize)]
struct TitleBody {
    #[serde(alias = "name")]
    title: Option<String>,
}
#[derive(Deserialize)]
struct RenameBody {
    title: String,
}
#[derive(Deserialize)]
struct RunBody {
    #[serde(default)]
    text: String,
    #[serde(default)]
    attachments: Vec<String>,
    #[serde(default)]
    options: RunOptions,
    #[serde(default)]
    client: Option<Value>,
}
#[derive(Default, Deserialize)]
struct ResumeBody {
    #[serde(default)]
    options: RunOptions,
    #[serde(default)]
    client: Option<Value>,
}
#[derive(Deserialize)]
struct TextBody {
    #[serde(default)]
    text: String,
    #[serde(default)]
    attachments: Vec<String>,
    #[serde(default)]
    client: Option<Value>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelBody {
    #[serde(default)]
    client: Option<Value>,
}
/// The optional `client: {id, name}` of a command (docs/session-api.md).
fn client(value: &Option<Value>) -> std::result::Result<Option<ClientInfo>, ApiError> {
    client_param(value.as_ref())
        .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))
}
async fn create_session(
    State(state): State<Arc<WebState>>,
    input: std::result::Result<Json<TitleBody>, JsonRejection>,
) -> std::result::Result<Json<Session>, ApiError> {
    let title = title(body(input)?.title)?;
    let created = state.clone();
    let session = blocking(move || {
        let (store, strategy) = (&created.store, created.services.context_strategy);
        match created.services.storage_policy {
            Some(policy) => {
                store.create_session_with_policy(title, strategy, &created.config.workspace, policy)
            }
            None => store.create_session_with_strategy_and_workspace(
                title,
                strategy,
                &created.config.workspace,
            ),
        }
    })
    .await?;
    if let Some(scope) = &state.services.session_scope {
        scope.adopt_idle_root(&session.id);
    }
    state.refresh.notify_one();
    let _ = state.events.send(HostEvent::Reset);
    Ok(Json(session))
}
async fn rename_session(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    input: std::result::Result<Json<RenameBody>, JsonRejection>,
) -> std::result::Result<Json<Value>, ApiError> {
    let input = body(input)?;
    command_request(&state, &id, SlashCommand::Rename(&input.title))
        .await
        .map(Json)
}
/// Runs one host command for a web request, then tells the page to reload.
async fn command_request(
    state: &WebState,
    id: &str,
    command: SlashCommand<'_>,
) -> std::result::Result<Value, ApiError> {
    let value = state.control.execute(Some(id), command).await?;
    state.refresh.notify_one();
    let _ = state.events.send(HostEvent::Reset);
    Ok(value)
}
/// A host command typed as the input text. Attachments make it a prompt, so
/// `Ok(None)` means the text is an ordinary prompt.
async fn command_text(
    state: &WebState,
    id: &str,
    text: &str,
    attachments: &[String],
) -> std::result::Result<Option<Value>, ApiError> {
    if !attachments.is_empty() {
        return Ok(None);
    }
    match SlashCommand::parse(text) {
        Some(command) => command_request(state, id, command).await.map(Some),
        None => Ok(None),
    }
}
fn ensure_workspace(state: &WebState, session: &str) -> std::result::Result<(), ApiError> {
    state
        .control
        .ensure_workspace(session)
        .map_err(ApiError::from)
}

async fn run(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    input: std::result::Result<Json<RunBody>, JsonRejection>,
) -> std::result::Result<Json<Value>, ApiError> {
    ensure_workspace(&state, &id)?;
    let input = body(input)?;
    if let Some(value) = command_text(&state, &id, &input.text, &input.attachments).await? {
        return Ok(Json(value));
    }
    let mut options = input.options;
    options.client = client(&input.client)?;
    let text = prepare_input(state.clone(), id.clone(), input.text, input.attachments).await?;
    options.origin = MessageOrigin::Typed;
    // Admission writes the accepted input to SQLite: run it on the blocking pool.
    let admitting = (state.clone(), id.clone());
    let run = blocking(move || {
        let (state, id) = admitting;
        match &state.services.session_scope {
            Some(scope) => scope.admit(&state.store, &id, false, || {
                state.harness.start(&id, text, options)
            }),
            None => state.harness.start(&id, text, options),
        }
    })
    .await?;
    if let Some(wake) = &state.wake {
        wake.user_input(&id);
    }
    state.refresh.notify_one();
    Ok(Json(metadata(&run)?))
}
async fn resume(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    input: std::result::Result<Json<ResumeBody>, JsonRejection>,
) -> std::result::Result<Json<Value>, ApiError> {
    ensure_workspace(&state, &id)?;
    let input = body(input)?;
    let mut options = input.options;
    options.client = client(&input.client)?;
    let admitting = state.clone();
    let run = blocking(move || {
        let state = admitting;
        match &state.services.session_scope {
            Some(scope) => scope.admit(&state.store, &id, false, || {
                state.harness.start_resume(&id, options)
            }),
            None => state.harness.start_resume(&id, options),
        }
    })
    .await?;
    state.refresh.notify_one();
    Ok(Json(metadata(&run)?))
}
async fn steer(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    input: std::result::Result<Json<TextBody>, JsonRejection>,
) -> std::result::Result<Json<Value>, ApiError> {
    ensure_workspace(&state, &id)?;
    let input = body(input)?;
    if let Some(value) = command_text(&state, &id, &input.text, &input.attachments).await? {
        return Ok(Json(value));
    }
    let origin = client(&input.client)?;
    let text = prepare_input(state.clone(), id.clone(), input.text, input.attachments).await?;
    let queued = state.harness.steer_from(&id, text, origin)?;
    if let Some(wake) = &state.wake {
        wake.user_input(&id);
    }
    state.refresh.notify_one();
    Ok(Json(
        serde_json::to_value(queued).map_err(anyhow::Error::from)?,
    ))
}
async fn followup(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    input: std::result::Result<Json<TextBody>, JsonRejection>,
) -> std::result::Result<Json<Value>, ApiError> {
    ensure_workspace(&state, &id)?;
    let input = body(input)?;
    if let Some(value) = command_text(&state, &id, &input.text, &input.attachments).await? {
        return Ok(Json(value));
    }
    let origin = client(&input.client)?;
    let text = prepare_input(state.clone(), id.clone(), input.text, input.attachments).await?;
    let queued = state.harness.followup_from(&id, text, origin)?;
    if let Some(wake) = &state.wake {
        wake.user_input(&id);
    }
    state.refresh.notify_one();
    Ok(Json(
        serde_json::to_value(queued).map_err(anyhow::Error::from)?,
    ))
}
async fn cancel(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    input: std::result::Result<Json<CancelBody>, JsonRejection>,
) -> std::result::Result<Json<Value>, ApiError> {
    // The workspace check answers before the body is parsed, like run and steer.
    ensure_workspace(&state, &id)?;
    let origin = client(&body(input)?.client)?;
    let requested = state.control.cancel(&id, origin).await?;
    Ok(Json(json!({"requested":requested})))
}
/// Owned command jobs of one session (`ProcessTools::jobs`). Ownership is the
/// session id the jobs were started under, so other sessions' jobs never appear.
async fn list_processes(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
) -> std::result::Result<Json<Value>, ApiError> {
    blocking(move || {
        state.store.session(&id)?;
        match &state.services.processes {
            Some(processes) => processes.jobs(&id),
            None => Ok(json!({"jobs": []})),
        }
    })
    .await
    .map(Json)
}
/// Moves a session's blocked foreground command to background; the waiting call
/// returns with its output so far. Another session's handle is reported as unknown.
async fn background_process(
    State(state): State<Arc<WebState>>,
    Path((id, handle)): Path<(String, String)>,
) -> std::result::Result<Json<Value>, ApiError> {
    let reply = blocking({
        let state = state.clone();
        move || {
            state.store.session(&id)?;
            state
                .services
                .processes
                .as_ref()
                .context("Processes are unavailable")?
                .background(&id, &handle)
        }
    })
    .await?;
    state.refresh.notify_one();
    Ok(Json(reply))
}
#[derive(Deserialize)]
struct DecisionBody {
    decision: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    client: Option<Value>,
}
async fn approvals(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
) -> std::result::Result<Json<Vec<ApprovalRecord>>, ApiError> {
    blocking(move || {
        state.store.session(&id)?;
        state.harness.pending_approvals(&id)
    })
    .await
    .map(Json)
}
async fn decide_approval(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    input: std::result::Result<Json<DecisionBody>, JsonRejection>,
) -> std::result::Result<Json<ApprovalRecord>, ApiError> {
    let input = body(input)?;
    let origin = client(&input.client)?;
    let decision = Decision::parse(&input.decision, input.reason)?;
    let record = state.harness.decide_from(&id, decision, origin)?;
    state.refresh.notify_one();
    Ok(Json(record))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuestionAnswerBody {
    question_id: String,
    text: String,
    #[serde(default)]
    client: Option<Value>,
}
async fn answer_question(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    input: std::result::Result<Json<QuestionAnswerBody>, JsonRejection>,
) -> std::result::Result<Json<Value>, ApiError> {
    ensure_workspace(&state, &id)?;
    let input = body(input)?;
    let origin = client(&input.client)?;
    let answering = state.clone();
    let answer = blocking(move || {
        let state = answering;
        if state.store.session(&id)?.parent_session_id.is_some() {
            return Err(ChildSessionRefusal.into());
        }
        let questions = state
            .services
            .questions
            .as_ref()
            .context("Question service unavailable")?;
        let answer = questions.answer(&state.harness, &id, &input.question_id, &input.text)?;
        if !answer.live {
            crate::question::continue_answer(&state.harness, &state.store, &id, &answer, origin)?;
        }
        Ok(answer)
    })
    .await?;
    state.refresh.notify_one();
    Ok(Json(json!({"question":answer.question,"live":answer.live})))
}
async fn fork(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    input: std::result::Result<Json<TitleBody>, JsonRejection>,
) -> std::result::Result<Json<Session>, ApiError> {
    ensure_workspace(&state, &id)?;
    let title = title(body(input)?.title)?;
    let forking = state.clone();
    let forked = blocking(move || forking.store.fork(&id, title)).await?;
    state.refresh.notify_one();
    let _ = state.events.send(HostEvent::Reset);
    Ok(Json(forked))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RewindBody {
    message_id: String,
}

async fn rewind(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    input: std::result::Result<Json<RewindBody>, JsonRejection>,
) -> std::result::Result<Json<Value>, ApiError> {
    let message_id = body(input)?.message_id;
    command_request(&state, &id, SlashCommand::Rewind(&message_id))
        .await
        .map(Json)
}

/// Logged events after `after`, or None when the log no longer holds them all
/// or there are more than the replay limit.
fn replay(harness: &Harness, after: u64) -> Result<Option<Vec<HarnessEvent>>> {
    if !harness.can_resume_from(after) {
        return Ok(None);
    }
    let mut events: Vec<HarnessEvent> = Vec::new();
    loop {
        let from = events.last().map_or(after, |event| event.sequence);
        let page = harness.events_after(from, None, bubblepaw_core::EVENTS_AFTER_LIMIT)?;
        let full = page.len() == bubblepaw_core::EVENTS_AFTER_LIMIT;
        events.extend(page);
        if events.len() > REPLAY_LIMIT {
            return Ok(None);
        }
        if !full {
            return Ok(Some(events));
        }
    }
}
fn sse_event(event: &HarnessEvent) -> Event {
    Event::default()
        .id(event.sequence.to_string())
        .json_data(event)
        .unwrap()
}
/// Live events, after a `reset` or, with `Last-Event-ID`, after replaying the
/// logged events the client missed (docs/web-contract.md).
async fn events_stream(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
) -> Sse<impl futures_util::Stream<Item = std::result::Result<Event, Infallible>>> {
    let receiver = state.events.subscribe();
    let stop = state.stop.clone();
    let last_id = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    let replayed = replayed_after(&state, last_id).await;
    // Before the first live event: a reset, or the replayed events.
    let (cursor, initial): (u64, VecDeque<Event>) = replayed.unwrap_or_else(|| {
        (
            0,
            VecDeque::from([Event::default().event("reset").data("{}")]),
        )
    });
    let stream = stream::unfold(
        (receiver, initial, cursor, stop),
        |(mut receiver, mut initial, mut cursor, stop)| async move {
            if let Some(event) = initial.pop_front() {
                return Some((Ok(event), (receiver, initial, cursor, stop)));
            }
            loop {
                let received = tokio::select! { _ = stop.cancelled() => return None, value = receiver.recv() => value };
                let event = match received {
                    Ok(HostEvent::Core(event)) => {
                        // Skip what the replay already sent.
                        if event.durable && event.sequence <= cursor
                            || !event.durable && event.sequence < cursor
                        {
                            continue;
                        }
                        cursor = cursor.max(event.sequence);
                        sse_event(event.as_ref())
                    }
                    Ok(HostEvent::Reset) | Err(broadcast::error::RecvError::Lagged(_)) => {
                        Event::default().event("reset").data("{}")
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                };
                return Some((Ok(event), (receiver, initial, cursor, stop)));
            }
        },
    );
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

fn authority(headers: &HeaderMap, port: u16) -> Option<String> {
    let host = headers.get("host")?.to_str().ok()?;
    let uri: Uri = format!("http://{host}").parse().ok()?;
    if uri.path() != "/" || uri.query().is_some() || uri.port_u16().unwrap_or(80) != port {
        return None;
    }
    let hostname = uri.host()?;
    let hostname = hostname.trim_start_matches('[').trim_end_matches(']');
    if hostname.eq_ignore_ascii_case("localhost")
        || hostname.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
    {
        Some(host.to_ascii_lowercase())
    } else {
        None
    }
}
async fn local_boundary(
    State(state): State<Arc<WebState>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let Some(host) = authority(request.headers(), state.port) else {
        return ApiError(
            StatusCode::FORBIDDEN,
            "Host must match the local server port".into(),
        )
        .into_response();
    };
    if let Some(origin) = request.headers().get("origin") {
        let expected = format!("http://{host}");
        if origin
            .to_str()
            .ok()
            .is_none_or(|origin| !origin.eq_ignore_ascii_case(&expected))
        {
            return ApiError(
                StatusCode::FORBIDDEN,
                "Origin must match this local server".into(),
            )
            .into_response();
        }
    }
    let segments: Vec<_> = request.uri().path().split('/').collect();
    let upload = segments.len() == 5
        && segments[1] == "api"
        && segments[2] == "sessions"
        && segments[4] == "attachments";
    let expected_content_type = if upload {
        "application/octet-stream"
    } else {
        "application/json"
    };
    if matches!(
        *request.method(),
        Method::POST | Method::PUT | Method::DELETE
    ) && request
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_none_or(|value| {
            !value
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case(expected_content_type)
        })
    {
        return ApiError(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!("{} requires {expected_content_type}", request.method()),
        )
        .into_response();
    }
    let api = request.uri().path().starts_with("/api/");
    let mut response = next.run(request).await;
    if api {
        response.headers_mut().insert(
            "cache-control",
            axum::http::HeaderValue::from_static("no-store"),
        );
    }
    if api
        && !response.status().is_success()
        && response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_none_or(|value| !value.starts_with("application/json"))
    {
        return ApiError(
            response.status(),
            response
                .status()
                .canonical_reason()
                .unwrap_or("Request failed")
                .into(),
        )
        .into_response();
    }
    if api
        && response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/html"))
    {
        return ApiError(StatusCode::NOT_FOUND, "Unknown API endpoint".into()).into_response();
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use bubblepaw_core::{
        HarnessConfig, ModelRequest, ModelResponse, Provider, TextCallback, Tool, ToolCall,
        ToolContext, ToolDefinition, ToolRegistry, ToolResult,
    };
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
        let control = SessionControl::new(harness.clone(), store.clone(), "fixture");
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
            serving: Mutex::new(ServingView::new().unwrap()),
            transient: Mutex::default(),
            events,
            refresh: Arc::new(Notify::new()),
            stop: CancellationToken::new(),
            services: WebServices::default(),
            control,
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
        let control = SessionControl::new(harness.clone(), store.clone(), "fixture");
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
            serving: Mutex::new(ServingView::new().unwrap()),
            transient: Mutex::default(),
            events,
            refresh: Arc::new(Notify::new()),
            stop: CancellationToken::new(),
            services: WebServices::default(),
            control,
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
