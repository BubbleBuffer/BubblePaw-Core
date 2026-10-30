//! The bridge from HTTP handlers to the shared host table. Each request builds
//! its context here and makes one dispatch per route.
use super::{WebState, errors::ApiError, usage_settings};
use crate::{
    host_api::{self, HostCtx, Profile, ReadScope},
    input_files::InputLimits,
    session_scope::SessionScope,
};
use axum::http::StatusCode;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::path::PathBuf;

pub(super) fn host_ctx(state: &WebState) -> HostCtx {
    let services = &state.services;
    let mut ctx = HostCtx::new(
        state.harness.clone(),
        state.store.clone(),
        PathBuf::from(&state.config.workspace),
    );
    ctx.read_scope = services.read_scope.clone().unwrap_or(ReadScope::Host);
    ctx.scope = services
        .session_scope
        .clone()
        .unwrap_or_else(SessionScope::all);
    ctx.context_strategy = services.context_strategy;
    ctx.storage_policy = services.storage_policy;
    ctx.input_limits = services
        .input_files
        .as_ref()
        .map_or_else(InputLimits::default, |files| files.limits());
    ctx.usage = usage_settings(state);
    ctx.config = serde_json::to_value(&state.config).unwrap_or_else(|_| json!({}));
    ctx.providers = services.provider_setup.clone();
    ctx.provider_config = services.providers.clone();
    ctx.live_config = services.live_config.clone();
    ctx.agents = services.agents.clone();
    ctx.repl = services.repl.clone();
    ctx.artifacts = services.artifacts.clone();
    ctx.input_files = services.input_files.clone();
    ctx.processes = services.processes.clone();
    ctx.questions = services.questions.clone();
    ctx.wake = state.wake.clone();
    ctx.views = Some(state.serving.clone());
    ctx
}

/// One host method for one route. Failures keep the host's status and message.
pub(super) async fn call(state: &WebState, method: &str, params: Value) -> Result<Value, ApiError> {
    Ok(host_api::dispatch(&host_ctx(state), Profile::Full, method, params).await?)
}

/// Decodes a host result into the typed view the handler shapes. A mismatch is
/// an adapter fault, so it is a server error.
pub(super) fn decode<T: DeserializeOwned>(value: Value) -> Result<T, ApiError> {
    serde_json::from_value(value)
        .map_err(|error| ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))
}

/// Tells the page that session state changed. Reset also tells open streams
/// to restart their live view.
pub(super) fn changed(state: &WebState, reset: bool) {
    state.refresh.notify_one();
    if reset {
        let _ = state.events.send(super::HostEvent::Reset);
    }
}
