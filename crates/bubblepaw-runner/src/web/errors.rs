//! Maps route failures to HTTP status codes. The text rules here are the
//! contract in docs/web-contract.md (Errors); typed errors are matched first.
use crate::session_control::WorkspaceMismatch;
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use std::fmt;

#[derive(Debug)]
pub(super) struct ApiError(pub(super) StatusCode, pub(super) String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (self.0, Json(json!({"error": self.1}))).into_response();
        response.headers_mut().insert(
            "cache-control",
            axum::http::HeaderValue::from_static("no-store"),
        );
        response
    }
}

/// A valid session asked to answer a question in the child role. Answers
/// come from the direct parent through `agent_send`, so this is a conflict
/// with the session's role, not a malformed request.
#[derive(Debug)]
pub(super) struct ChildSessionRefusal;

impl fmt::Display for ChildSessionRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .write_str("Child questions must be answered by their direct parent through agent_send")
    }
}

impl std::error::Error for ChildSessionRefusal {}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        let text = error.to_string();
        let status = if error.downcast_ref::<WorkspaceMismatch>().is_some()
            || error.downcast_ref::<ChildSessionRefusal>().is_some()
        {
            StatusCode::CONFLICT
        } else if error
            .downcast_ref::<rusqlite::Error>()
            .is_some_and(|error| matches!(error, rusqlite::Error::QueryReturnedNoRows))
        {
            StatusCode::NOT_FOUND
        } else if text.contains("active")
            || text.contains("unresolved tool")
            || text.contains("shutting down")
        {
            StatusCode::CONFLICT
        } else if text.contains("unknown or expired") || text.contains("another conversation") {
            StatusCode::NOT_FOUND
        } else if text.contains("size limit")
            || text.contains("read only")
            || text.contains("unsupported handle")
            || text.contains("not available in this host")
        {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        Self(status, text)
    }
}
