//! Local-only checks for the HTTP host: the Host header must name this machine's
//! port, a browser Origin must match it, and mutating requests must send the
//! expected content type. API responses are never cached.
use super::{WebState, errors::ApiError};
use axum::{
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::{net::IpAddr, sync::Arc};

fn authority(headers: &HeaderMap, port: u16) -> Option<String> {
    let host = headers.get("host")?.to_str().ok()?;
    let uri: Uri = format!("http://{host}").parse().ok()?;
    if uri.path() != "/" || uri.query().is_some() || uri.port_u16().unwrap_or(80) != port {
        return None;
    }
    let hostname = uri.host()?;
    let hostname = hostname.trim_start_matches('[').trim_end_matches(']');
    if hostname.eq_ignore_ascii_case("localhost")
        || hostname.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
    {
        Some(host.to_ascii_lowercase())
    } else {
        None
    }
}
pub(super) async fn local_boundary(
    State(state): State<Arc<WebState>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let Some(host) = authority(request.headers(), state.port) else {
        return ApiError(
            StatusCode::FORBIDDEN,
            "Host must match the local server port".into(),
        )
        .into_response();
    };
    if let Some(origin) = request.headers().get("origin") {
        let expected = format!("http://{host}");
        if origin
            .to_str()
            .ok()
            .is_none_or(|origin| !origin.eq_ignore_ascii_case(&expected))
        {
            return ApiError(
                StatusCode::FORBIDDEN,
                "Origin must match this local server".into(),
            )
            .into_response();
        }
    }
    let segments: Vec<_> = request.uri().path().split('/').collect();
    let upload = segments.len() == 5
        && segments[1] == "api"
        && segments[2] == "sessions"
        && segments[4] == "attachments";
    let expected_content_type = if upload {
        "application/octet-stream"
    } else {
        "application/json"
    };
    if matches!(
        *request.method(),
        Method::POST | Method::PUT | Method::DELETE
    ) && request
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_none_or(|value| {
            !value
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case(expected_content_type)
        })
    {
        return ApiError(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!("{} requires {expected_content_type}", request.method()),
        )
        .into_response();
    }
    let api = request.uri().path().starts_with("/api/");
    let mut response = next.run(request).await;
    if api {
        response.headers_mut().insert(
            "cache-control",
            axum::http::HeaderValue::from_static("no-store"),
        );
    }
    if api
        && !response.status().is_success()
        && response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_none_or(|value| !value.starts_with("application/json"))
    {
        return ApiError(
            response.status(),
            response
                .status()
                .canonical_reason()
                .unwrap_or("Request failed")
                .into(),
        )
        .into_response();
    }
    if api
        && response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/html"))
    {
        return ApiError(StatusCode::NOT_FOUND, "Unknown API endpoint".into()).into_response();
    }
    response
}
