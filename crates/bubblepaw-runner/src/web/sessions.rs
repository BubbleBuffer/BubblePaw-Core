//! Session routes. Each handler makes one host call. The session view is the
//! host projection with this page's live paragraphs and sequence overlaid.
use super::{
    LiveRound, WebState, body,
    errors::ApiError,
    host::{call, changed, decode},
};
use crate::{
    host_api::{
        attachments::FileData,
        views::{SessionSummary, SessionView, SessionViewResult},
    },
    input_files::InputLimits,
};
use axum::{
    Json,
    body::{Body, to_bytes},
    extract::{Path, Query, State, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Deserialize)]
pub(super) struct TitleBody {
    #[serde(alias = "name")]
    title: Option<String>,
}
#[derive(Deserialize)]
pub(super) struct RenameBody {
    title: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RewindBody {
    message_id: String,
}
#[derive(Deserialize)]
pub(super) struct DecisionBody {
    decision: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    client: Option<Value>,
}
#[derive(Deserialize)]
pub(super) struct UploadQuery {
    name: String,
}

pub(super) async fn get_config(
    State(state): State<Arc<WebState>>,
) -> Result<Json<Value>, ApiError> {
    call(&state, "config.get", json!({})).await.map(Json)
}

pub(super) async fn sessions(
    State(state): State<Arc<WebState>>,
) -> Result<Json<Vec<SessionSummary>>, ApiError> {
    decode(call(&state, "session.sidebar", json!({})).await?).map(Json)
}

pub(super) async fn create_session(
    State(state): State<Arc<WebState>>,
    input: std::result::Result<Json<TitleBody>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let title = body(input)?.title;
    let created = call(&state, "session.open", json!({"title": title})).await?;
    changed(&state, true);
    Ok(Json(created))
}

pub(super) async fn session(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
) -> Result<Json<SessionView>, ApiError> {
    loop {
        let SessionViewResult { mut view, revision } =
            decode(call(&state, "session.view", json!({"session_id": id})).await?)?;
        let transient = state.transient.lock().unwrap();
        if state.store.revision() != revision {
            drop(transient);
            continue;
        }
        view.event_sequence = transient.sequence;
        view.seq = transient.sequence.unwrap_or_default();
        view.live_seen = transient.live.get(&id).copied().unwrap_or_default();
        if let Some(round) = transient
            .sessions
            .get(&id)
            .filter(|round| Some(&round.run_id) == view.active_run_id.as_ref())
        {
            overlay_round(&mut view, round);
        }
        return Ok(Json(view));
    }
}

/// Adds the live paragraphs of the active run, unless a persisted assistant
/// message already shows a newer phase than the last tool call.
fn overlay_round(view: &mut SessionView, round: &LiveRound) {
    let started = view
        .runs
        .iter()
        .find(|run| run["id"].as_str() == Some(&round.run_id))
        .and_then(|run| run["started_at"].as_str())
        .unwrap_or("");
    let persisted = view.messages.iter().rev().find(|message| {
        message.role == bubblepaw_core::Role::Assistant && message.created_at.as_str() >= started
    });
    let phase_is_current = persisted.is_none_or(|message| {
        round
            .last_tool_call
            .as_ref()
            .is_some_and(|id| message.tool_calls.iter().any(|tool| &tool.id == id))
    });
    if phase_is_current {
        view.live_paragraphs = round.paragraphs.iter().cloned().collect();
    }
    view.limits.live_paragraphs_truncated = round.truncated;
}

pub(super) async fn rename_session(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    input: std::result::Result<Json<RenameBody>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let input = body(input)?;
    let renamed = call(
        &state,
        "session.rename",
        json!({"session_id": id, "title": input.title}),
    )
    .await?;
    changed(&state, true);
    Ok(Json(renamed))
}

pub(super) async fn fork(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    input: std::result::Result<Json<TitleBody>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let title = body(input)?.title;
    let forked = call(
        &state,
        "session.fork",
        json!({"session_id": id, "title": title}),
    )
    .await?;
    changed(&state, true);
    Ok(Json(forked))
}

pub(super) async fn rewind(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    input: std::result::Result<Json<RewindBody>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let message_id = body(input)?.message_id;
    let rewound = call(
        &state,
        "session.rewind",
        json!({"session_id": id, "message_id": message_id}),
    )
    .await?;
    changed(&state, true);
    Ok(Json(rewound))
}

pub(super) async fn list_processes(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    call(&state, "process.list", json!({"session_id": id}))
        .await
        .map(Json)
}

/// Moves a session's blocked foreground command to background. Another
/// session's handle is reported as unknown by the host.
pub(super) async fn background_process(
    State(state): State<Arc<WebState>>,
    Path((id, handle)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let reply = call(
        &state,
        "process.background",
        json!({"session_id": id, "handle": handle}),
    )
    .await?;
    state.refresh.notify_one();
    Ok(Json(reply))
}

pub(super) async fn approvals(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    call(&state, "approval.list", json!({"session_id": id}))
        .await
        .map(Json)
}

pub(super) async fn decide_approval(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    input: std::result::Result<Json<DecisionBody>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let input = body(input)?;
    let record = call(
        &state,
        "approval.decide",
        json!({"approval_id": id, "decision": input.decision, "reason": input.reason, "client": input.client}),
    )
    .await?;
    state.refresh.notify_one();
    Ok(Json(record))
}

pub(super) async fn environments(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    call(&state, "environment.inspect", json!({"session_id": id}))
        .await
        .map(Json)
}

pub(super) async fn control_environment(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    input: std::result::Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let input = body(input)?;
    let reply = call(
        &state,
        "environment.control",
        json!({"session_id": id, "control": input}),
    )
    .await?;
    changed(&state, true);
    Ok(Json(reply))
}

/// The body limits. Without the attachment service the host default applies:
/// the host answers a missing session or service, so this only sizes the body.
fn attachment_limits(state: &WebState) -> InputLimits {
    state
        .services
        .input_files
        .as_ref()
        .map_or_else(InputLimits::default, |files| files.limits())
}

pub(super) async fn upload_attachment(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    Query(query): Query<UploadQuery>,
    request: axum::extract::Request,
) -> Result<Json<Value>, ApiError> {
    let limit = attachment_limits(&state).max_file_bytes as usize;
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
    call(
        &state,
        "attachments.add",
        json!({"session_id": id, "name": query.name, "data": STANDARD.encode(&bytes)}),
    )
    .await
    .map(Json)
}

pub(super) async fn download_attachment(
    State(state): State<Arc<WebState>>,
    Path((id, file)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let read: FileData = decode(
        call(
            &state,
            "attachments.read",
            json!({"session_id": id, "id": file}),
        )
        .await?,
    )?;
    let bytes = STANDARD
        .decode(read.data)
        .map_err(|error| ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    let name = read.file.name;
    let ascii: String = name
        .chars()
        .map(|c| if c.is_ascii() { c } else { '_' })
        .collect();
    let encoded: String = name
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

pub(super) async fn artifact(
    State(state): State<Arc<WebState>>,
    Path((session, id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let read: crate::host_api::artifacts::ArtifactData = decode(
        call(
            &state,
            "artifacts.read",
            json!({"session_id": session, "id": id}),
        )
        .await?,
    )?;
    let bytes = STANDARD
        .decode(read.data)
        .map_err(|error| ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    let meta = read.artifact;
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
