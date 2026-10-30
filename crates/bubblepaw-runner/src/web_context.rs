//! Thin GET mappings into the host's context reads and history pagination.
use super::{WebState, errors::ApiError, host::call};
use axum::{
    Json, Router,
    extract::{Path, Query, State, rejection::QueryRejection},
    http::StatusCode,
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};

pub(super) fn routes() -> Router<Arc<WebState>> {
    Router::new()
        .route("/api/sessions/{id}/context/{resource}", get(context))
        .route("/api/sessions/{id}/history/range", get(history_range))
}

async fn context(
    State(state): State<Arc<WebState>>,
    Path((id, resource)): Path<(String, String)>,
    query: std::result::Result<Query<HashMap<String, String>>, QueryRejection>,
) -> std::result::Result<Json<Value>, ApiError> {
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
    let mut params = json!(query);
    params["session_id"] = json!(id);
    call(&state, method, params).await.map(Json)
}

async fn history_range(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    query: std::result::Result<Query<crate::context_reads::HistoryRange>, QueryRejection>,
) -> std::result::Result<Json<Value>, ApiError> {
    let Query(query) =
        query.map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.body_text()))?;
    let mut params = json!(query);
    params["session_id"] = json!(id);
    call(&state, "history.range", params).await.map(Json)
}

#[derive(Deserialize)]
pub(super) struct MessagesQuery {
    before: Option<String>,
    limit: Option<usize>,
}
pub(super) async fn messages(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    query: std::result::Result<Query<MessagesQuery>, QueryRejection>,
) -> std::result::Result<Json<Value>, ApiError> {
    let Query(query) =
        query.map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.body_text()))?;
    call(
        &state,
        "history.messages",
        json!({"session_id": id, "before": query.before, "limit": query.limit}),
    )
    .await
    .map(Json)
}
