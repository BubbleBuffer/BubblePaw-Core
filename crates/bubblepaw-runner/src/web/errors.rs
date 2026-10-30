//! HTTP view of a host failure. `HostError` owns the status and message for
//! every transport; this type only adds the no-store header.
use crate::host_api::HostError;
use axum::{
    Json,
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::json;

#[derive(Debug)]
pub(super) struct ApiError(pub(super) StatusCode, pub(super) String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (self.0, Json(json!({"error": self.1}))).into_response();
        response
            .headers_mut()
            .insert("cache-control", HeaderValue::from_static("no-store"));
        response
    }
}

impl From<HostError> for ApiError {
    fn from(error: HostError) -> Self {
        let status =
            StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        Self(status, error.message)
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        HostError::from(error).into()
    }
}
