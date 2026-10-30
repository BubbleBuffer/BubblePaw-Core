//! HTTP question routes. The host answers; a child answer is an explicit host
//! user action, and both are refused or accepted by `host_api::questions`.
use super::{WebState, body, errors::ApiError, host::call};
use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct QuestionAnswerBody {
    question_id: String,
    text: String,
    #[serde(default)]
    client: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ChildQuestionAnswerBody {
    question_id: String,
    text: String,
}

pub(super) async fn answer_child_question(
    State(state): State<Arc<WebState>>,
    Path((parent, child)): Path<(String, String)>,
    input: std::result::Result<Json<ChildQuestionAnswerBody>, JsonRejection>,
) -> std::result::Result<Json<Value>, ApiError> {
    let input = body(input)?;
    let reply = call(
        &state,
        "question.answer_child",
        json!({"parent": parent, "child": child, "question_id": input.question_id, "text": input.text}),
    )
    .await?;
    state.refresh.notify_one();
    Ok(Json(reply))
}

pub(super) async fn answer_question(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    input: std::result::Result<Json<QuestionAnswerBody>, JsonRejection>,
) -> std::result::Result<Json<Value>, ApiError> {
    let input = body(input)?;
    let reply = call(
        &state,
        "question.answer",
        json!({"session_id": id, "question_id": input.question_id, "text": input.text, "client": input.client}),
    )
    .await?;
    state.refresh.notify_one();
    Ok(Json(reply))
}
