//! HTTP question answer routes, including explicit host user answers to children.
use super::errors::ChildSessionRefusal;
use super::*;

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
    ensure_workspace(&state, &parent)?;
    ensure_workspace(&state, &child)?;
    let input = body(input)?;
    let questions = state
        .services
        .questions
        .as_ref()
        .context("Question service unavailable")?;
    questions
        .answer_child_as_user(&parent, &child, &input.question_id, &input.text)
        .await
        .map_err(|error| {
            use crate::question::ChildQuestionAnswerError;
            let status = match &error {
                ChildQuestionAnswerError::NotFound => StatusCode::NOT_FOUND,
                ChildQuestionAnswerError::Conflict => StatusCode::CONFLICT,
                ChildQuestionAnswerError::Invalid => StatusCode::BAD_REQUEST,
                ChildQuestionAnswerError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            };
            ApiError(status, error.to_string())
        })?;
    state.refresh.notify_one();
    Ok(Json(json!({"answered":true})))
}

pub(super) async fn answer_question(
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
