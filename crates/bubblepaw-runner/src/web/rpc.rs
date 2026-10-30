//! `POST /rpc`: the stdio JSON-RPC protocol over HTTP. Requests run through the
//! shared host table with the full profile; a notification gets no body.
use super::{WebState, host::host_ctx};
use crate::host_api::{Profile, rpc::handle_bytes};
use axum::{
    Json,
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use std::sync::Arc;

pub(super) async fn rpc(State(state): State<Arc<WebState>>, body: Bytes) -> Response {
    match handle_bytes(&host_ctx(&state), Profile::Full, &body).await {
        Some(reply) => Json(reply).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}
