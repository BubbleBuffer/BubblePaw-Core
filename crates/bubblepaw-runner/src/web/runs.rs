//! Run routes. Each one is a single host call. The table admits a typed slash
//! command in the text as a command and any other text as input, so these
//! handlers only shape the reply.
use super::{
    WebState, body,
    errors::ApiError,
    host::{call, changed, decode},
};
use crate::host_api::views::run_metadata;
use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
};
use bubblepaw_core::{QueuedInput, Run, RunOptions};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Deserialize)]
pub(super) struct RunBody {
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
pub(super) struct ResumeBody {
    #[serde(default)]
    options: RunOptions,
    #[serde(default)]
    client: Option<Value>,
}
#[derive(Deserialize)]
pub(super) struct TextBody {
    #[serde(default)]
    text: String,
    #[serde(default)]
    attachments: Vec<String>,
    #[serde(default)]
    client: Option<Value>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CancelBody {
    #[serde(default)]
    client: Option<Value>,
}

/// Run metadata omits the run text (docs/session-api.md).
fn run_json(value: Value) -> Result<Json<Value>, ApiError> {
    let run: Run = decode(value)?;
    Ok(Json(run_metadata(&run)?))
}

/// `run.start` answers a prompt with its run, and a slash command with the
/// command's own result. A run is shaped as metadata; a command result changed
/// the session, so open streams restart their live view.
fn started(state: &WebState, value: Value) -> Result<Json<Value>, ApiError> {
    match serde_json::from_value::<Run>(value.clone()) {
        Ok(run) => {
            state.refresh.notify_one();
            Ok(Json(run_metadata(&run)?))
        }
        Err(_) => {
            changed(state, true);
            Ok(Json(value))
        }
    }
}

/// `run.steer` and `run.follow_up` answer with the queued input, or with a slash
/// command's result. Only a command result resets open streams.
fn queued(state: &WebState, value: Value) -> Json<Value> {
    if serde_json::from_value::<QueuedInput>(value.clone()).is_ok() {
        state.refresh.notify_one();
    } else {
        changed(state, true);
    }
    Json(value)
}

pub(super) async fn run(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    input: std::result::Result<Json<RunBody>, JsonRejection>,
) -> std::result::Result<Json<Value>, ApiError> {
    let input = body(input)?;
    let reply = call(
        &state,
        "run.start",
        json!({
            "session_id": id,
            "text": input.text,
            "attachments": input.attachments,
            "options": input.options,
            "client": input.client,
        }),
    )
    .await?;
    started(&state, reply)
}

pub(super) async fn resume(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    input: std::result::Result<Json<ResumeBody>, JsonRejection>,
) -> std::result::Result<Json<Value>, ApiError> {
    let input = body(input)?;
    let resumed = call(
        &state,
        "run.resume",
        json!({"session_id": id, "options": input.options, "client": input.client}),
    )
    .await?;
    state.refresh.notify_one();
    run_json(resumed)
}

pub(super) async fn steer(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    input: std::result::Result<Json<TextBody>, JsonRejection>,
) -> std::result::Result<Json<Value>, ApiError> {
    let input = body(input)?;
    let reply = call(
        &state,
        "run.steer",
        json!({"session_id": id, "text": input.text, "attachments": input.attachments, "client": input.client}),
    )
    .await?;
    Ok(queued(&state, reply))
}

pub(super) async fn followup(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    input: std::result::Result<Json<TextBody>, JsonRejection>,
) -> std::result::Result<Json<Value>, ApiError> {
    let input = body(input)?;
    let reply = call(
        &state,
        "run.follow_up",
        json!({"session_id": id, "text": input.text, "attachments": input.attachments, "client": input.client}),
    )
    .await?;
    Ok(queued(&state, reply))
}

pub(super) async fn cancel(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    input: std::result::Result<Json<CancelBody>, JsonRejection>,
) -> std::result::Result<Json<Value>, ApiError> {
    let client = body(input)?.client;
    call(
        &state,
        "run.cancel",
        json!({"session_id": id, "client": client}),
    )
    .await
    .map(Json)
}
