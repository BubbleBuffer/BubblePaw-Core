//! Bounded persisted projections. Transports may cache them by revision and
//! overlay their own transient paragraphs without reading history again.
pub use super::views_cache::ServingView;
use super::*;
use bubblepaw_core::{ApprovalRecord, Message, QueuedInput, Run, Session};

pub const SESSION_LIMIT: usize = 64;
pub const MESSAGE_LIMIT: usize = 200;
pub const RUN_LIMIT: usize = 64;
pub const INPUT_LIMIT: usize = 128;
pub const OUTPUT_LIMIT: usize = 128;

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
    /// Run metadata omits the potentially large final text.
    pub runs: Vec<Value>,
    pub pending_inputs: Vec<QueuedInput>,
    pub active_run_id: Option<String>,
    pub live_paragraphs: Vec<String>,
    pub event_sequence: Option<u64>,
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub live_seen: u64,
    pub limits: ViewLimits,
    #[serde(default)]
    pub outputs: Vec<bubblepaw_core::OutputRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repl: Option<Value>,
    #[serde(default)]
    pub approvals: Vec<ApprovalRecord>,
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

pub fn run_metadata(run: &Run) -> Result<Value, HostError> {
    let mut value = encode(run)?;
    value
        .as_object_mut()
        .expect("Run serializes as an object")
        .remove("text");
    Ok(value)
}

/// The returned revision is for disposable cache invalidation, not a wire field.
pub(super) fn persisted_sidebar(ctx: &HostCtx) -> Result<(Vec<SessionSummary>, u64), HostError> {
    let revision = ctx.store.revision();
    let mut records = Vec::new();
    for record in ctx.store.visible_session_summaries(SESSION_LIMIT)? {
        if ctx.read_scope.allows(&ctx.store, &record.session.id)? {
            records.push(record);
        }
    }
    let mut summaries = records
        .iter()
        .map(|record| {
            summary(
                &record.session,
                record.message_count,
                record.last_message.as_ref(),
                record.latest_run.as_ref(),
            )
        })
        .collect::<Vec<_>>();
    for item in &mut summaries {
        // Keep cold archives cold when projecting the sidebar.
        if ctx.store.active_session_metadata(&item.id)?.is_some()
            && ctx
                .store
                .host_state(&item.id, "question")?
                .is_some_and(|question| question["status"] == "pending")
        {
            item.status = "waiting_for_answer".into();
        }
    }
    summaries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok((summaries, revision))
}

/// A bounded persisted session view, and the store revision used to build it.
/// Live paragraphs remain the adapter's disposable projection.
pub(super) fn persisted_session(ctx: &HostCtx, id: &str) -> Result<(SessionView, u64), HostError> {
    let snapshot = ctx
        .store
        .snapshot(id, MESSAGE_LIMIT, RUN_LIMIT, INPUT_LIMIT)?;
    let mut messages = snapshot.messages;
    for message in &mut messages {
        message.provider_state = None;
    }
    let mut view = SessionView {
        usage: Some(crate::usage::session_usage(
            &ctx.store,
            id,
            &ctx.usage_settings(),
        )?),
        todo: crate::todo::list(&ctx.store, id)?,
        question: ctx
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
        runs: snapshot
            .runs
            .iter()
            .map(run_metadata)
            .collect::<Result<_, _>>()
            .map_err(anyhow::Error::new)?,
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
        outputs: ctx.store.outputs(id, OUTPUT_LIMIT)?,
        repl: None,
    };
    if view.question.is_some() {
        view.session.status = "waiting_for_answer".into();
    }
    Ok((view, snapshot.revision))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionViewResult {
    pub view: SessionView,
    pub revision: u64,
}

pub fn sidebar(
    ctx: &HostCtx,
    _input: EmptyParams,
) -> Result<(Vec<SessionSummary>, u64), HostError> {
    match &ctx.views {
        Some(cache) => {
            let mut cache = cache
                .lock()
                .map_err(|_| HostError::internal("Serving view cache poisoned"))?;
            super::views_cache::sidebar(ctx, &mut cache)
        }
        None => persisted_sidebar(ctx),
    }
}
pub fn session(ctx: &HostCtx, input: SessionParams) -> Result<(SessionView, u64), HostError> {
    let id = &input.session_id;
    ctx.ensure_read_session(id)?;
    loop {
        // Cache before the event log matches background refresh's lock order.
        let ((mut view, revision), position) = match &ctx.views {
            Some(cache) => {
                let mut cache = cache
                    .lock()
                    .map_err(|_| HostError::internal("Serving view cache poisoned"))?;
                snapshot(ctx, id, Some(&mut cache))?
            }
            None => snapshot(ctx, id, None)?,
        };
        if ctx.store.revision() != revision {
            continue;
        }
        view.seq = position.seq;
        view.event_sequence = Some(position.seq);
        view.repl = ctx.repl.as_ref().map(|repl| repl.status(id));
        return Ok((view, revision));
    }
}
fn snapshot(
    ctx: &HostCtx,
    id: &str,
    cache: Option<&mut ServingView>,
) -> Result<((SessionView, u64), bubblepaw_core::LogPosition), HostError> {
    ctx.harness
        .snapshot_with(id, || {
            let (mut view, revision) = match cache {
                Some(cache) => {
                    super::views_cache::session(ctx, cache, id).map_err(anyhow::Error::new)?
                }
                None => persisted_session(ctx, id).map_err(anyhow::Error::new)?,
            };
            view.approvals = ctx.harness.pending_approvals(id)?;
            Ok((view, revision))
        })
        .map_err(Into::into)
}
pub fn refresh_cached(ctx: &HostCtx) -> Result<(), HostError> {
    if let Some(cache) = &ctx.views {
        let mut cache = cache
            .lock()
            .map_err(|_| HostError::internal("Serving view cache poisoned"))?;
        super::views_cache::refresh(ctx, &mut cache)?;
    }
    Ok(())
}
pub(super) fn command(ctx: &HostCtx, method: MethodId, params: Value) -> Result<Value, HostError> {
    match method {
        MethodId::SessionView => {
            let (view, revision) = session(ctx, decode(params)?)?;
            encode(SessionViewResult { view, revision })
        }
        MethodId::SessionSidebar => encode(sidebar(ctx, decode(params)?)?.0),
        _ => unreachable!("view method table"),
    }
}
