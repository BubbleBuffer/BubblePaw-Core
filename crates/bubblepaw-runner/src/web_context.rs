//! Thin GET mappings into shared context reads, plus existing history pagination.
use super::*;

pub(super) fn routes() -> Router<Arc<WebState>> {
    Router::new()
        .route("/api/sessions/{id}/context/{resource}", get(context))
        .route("/api/sessions/{id}/history/range", get(history_range))
}

fn read_error(error: anyhow::Error) -> ApiError {
    if error
        .downcast_ref::<crate::context_reads::InvalidContextRead>()
        .is_some()
    {
        ApiError(StatusCode::BAD_REQUEST, error.to_string())
    } else {
        error.into()
    }
}

async fn serve(
    state: Arc<WebState>,
    id: String,
    method: &'static str,
    params: Value,
) -> std::result::Result<Json<Value>, ApiError> {
    blocking(move || {
        Ok(crate::context_reads::read(
            &state.store,
            &id,
            method,
            &params,
        ))
    })
    .await?
    .map(Json)
    .map_err(read_error)
}

async fn context(
    State(state): State<Arc<WebState>>,
    Path((id, resource)): Path<(String, String)>,
    query: std::result::Result<Query<HashMap<String, String>>, QueryRejection>,
) -> std::result::Result<Json<Value>, ApiError> {
    ensure_workspace(&state, &id)?;
    let method = match resource.as_str() {
        "view" => "context.view",
        "structure" => "context.structure",
        "summary" => "context.summary",
        "events" => "context.events",
        _ => {
            return Err(ApiError(
                StatusCode::NOT_FOUND,
                "Unknown context read".into(),
            ));
        }
    };
    let Query(query) =
        query.map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.body_text()))?;
    serve(state, id, method, json!(query)).await
}

async fn history_range(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    query: std::result::Result<Query<crate::context_reads::HistoryRange>, QueryRejection>,
) -> std::result::Result<Json<Value>, ApiError> {
    ensure_workspace(&state, &id)?;
    let Query(query) =
        query.map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.body_text()))?;
    serve(state, id, "history.range", json!(query)).await
}

#[derive(Deserialize)]
pub(super) struct MessagesQuery {
    before: Option<String>,
    limit: Option<usize>,
}
#[derive(Serialize)]
pub(super) struct MessagesPage {
    messages: Vec<Message>,
    has_more: bool,
}
pub(super) async fn messages(
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
