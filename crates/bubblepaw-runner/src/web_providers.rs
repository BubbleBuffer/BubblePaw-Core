//! Provider setup routes. Each route makes one host dispatch; `provider_setup`
//! owns the logic and `host_api::providers` the method names. This module maps
//! the request body to parameters and the host result to HTTP. Merge this
//! router before the host's local boundary.
pub use crate::provider_setup::ProviderSetup;
#[cfg(test)]
use crate::{
    config::EffectiveConfig,
    provider_setup::{
        ConfirmedHost, KINDS, auth_status, codex_auth_file, confirmed_host, loopback_url, model_url,
    },
};
use crate::{
    host_api::{HostCtx, HostError, Profile, dispatch},
    provider_setup::{ApiError, ApiResult, bad},
};
#[cfg(test)]
use anyhow::Result;
use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        Path, State,
        rejection::{BytesRejection, JsonRejection},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
#[cfg(test)]
use toml_edit::value;

/// The standalone router wraps its setup state in a host context. The store is
/// in memory because provider routes never read sessions.
pub fn router(state: ProviderSetup) -> Router {
    let store = bubblepaw_core::Store::in_memory().expect("in-memory store opens");
    let mut ctx = HostCtx::for_store(store, ".");
    ctx.providers = Some(state);
    Router::new()
        .route("/api/providers", get(list))
        .route("/api/providers/{name}", put(save).delete(remove))
        .route("/api/roles/main", put(save_main))
        .route("/api/providers/{name}/test", post(test))
        .route("/api/config/reload", post(reload))
        .with_state(Arc::new(ctx))
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}
/// Host failures keep their status and message; `HostError` maps every cause.
async fn call(ctx: &HostCtx, method: &str, params: Value) -> ApiResult<Value> {
    dispatch(ctx, Profile::Full, method, params)
        .await
        .map_err(http)
}
fn http(error: HostError) -> ApiError {
    let status = StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    ApiError(status, error.message)
}
fn body<T>(input: std::result::Result<Json<T>, JsonRejection>) -> ApiResult<T> {
    input.map(|Json(input)| input).map_err(|_| bad())
}

async fn list(State(ctx): State<Arc<HostCtx>>) -> ApiResult<Json<Value>> {
    call(&ctx, "providers.list", json!({})).await.map(Json)
}
async fn reload(State(ctx): State<Arc<HostCtx>>) -> ApiResult<Response> {
    let reply = call(&ctx, "providers.reload", json!({})).await?;
    // An unapplied reload keeps its diff and error in the body, with status 400.
    let status = if reply["applied"] == false {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::OK
    };
    Ok((status, Json(reply)).into_response())
}
async fn save(
    State(ctx): State<Arc<HostCtx>>,
    Path(name): Path<String>,
    input: std::result::Result<Json<Value>, JsonRejection>,
) -> ApiResult<Json<Value>> {
    let input = body(input)?;
    call(
        &ctx,
        "providers.save",
        json!({"name": name, "input": input}),
    )
    .await
    .map(Json)
}
async fn remove(
    State(ctx): State<Arc<HostCtx>>,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    call(&ctx, "providers.remove", json!({"name": name}))
        .await
        .map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MainInput {
    model: String,
}
async fn save_main(
    State(ctx): State<Arc<HostCtx>>,
    input: std::result::Result<Json<MainInput>, JsonRejection>,
) -> ApiResult<Json<Value>> {
    let model = body(input)?.model;
    call(&ctx, "roles.set_main", json!({"model": model}))
        .await
        .map(Json)
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TestInput {
    draft: Option<Value>,
}
async fn test(
    State(ctx): State<Arc<HostCtx>>,
    Path(name): Path<String>,
    input: std::result::Result<Bytes, BytesRejection>,
) -> ApiResult<Json<Value>> {
    let bytes = input.map_err(|_| bad())?;
    let input: TestInput = if bytes.is_empty() {
        TestInput::default()
    } else {
        serde_json::from_slice(&bytes).map_err(|_| bad())?
    };
    call(
        &ctx,
        "providers.test",
        json!({"name": name, "input": input.draft}),
    )
    .await
    .map(Json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::LoadOptions, provider::LiveProviderConfig};
    use std::sync::Arc;
    #[test]
    fn provider_save_preserves_invoking_environment_without_disclosing_it() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let file = workspace.path().join("config.toml");
        std::fs::write(
            &file,
            "[model]\nprovider='fixture'\nmain='fixture/model'\n[providers.fixture]\nkind='responses'\nbase_url='https://example.invalid/v1'\nkey_env='BP_WEB_SNAPSHOT_TEST_KEY'\n",
        )?;
        let secret = "synthetic-private-web-snapshot-key";
        let codex_home = workspace.path().join("client-codex");
        let launched = EffectiveConfig::load(LoadOptions {
            workspace: workspace.path().to_owned(),
            home: None,
            custom_file: Some(file),
            env: vec![
                ("BP_WEB_SNAPSHOT_TEST_KEY".into(), secret.into()),
                (
                    "CODEX_HOME".into(),
                    codex_home.to_string_lossy().into_owned(),
                ),
            ],
            cli: vec![],
        })?;
        assert!(launched.load_options().env.is_empty());
        let live = Arc::new(LiveProviderConfig::new(launched.clone())?);
        let setup = ProviderSetup::new(launched.load_options(), &launched)?.with_live(live.clone());
        let mut document = setup
            .0
            .document()
            .map_err(|error| anyhow::anyhow!(error.1))?;
        document["providers"]["fixture"]["base_url"] = value("https://example.invalid/v2");
        let candidate = setup
            .0
            .candidate(&document)
            .map_err(|error| anyhow::anyhow!(error.1))?;
        assert_eq!(
            candidate.environment("BP_WEB_SNAPSHOT_TEST_KEY").as_deref(),
            Some(secret)
        );
        assert!(!format!("{candidate:?}").contains(secret));
        let reply = setup
            .0
            .write_apply(&document, candidate)
            .map_err(|error| anyhow::anyhow!(error.1))?;
        let provider = reply["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|provider| provider["name"] == "fixture")
            .unwrap();
        assert_eq!(provider["key_present"], true);
        assert!(!reply.to_string().contains(secret));
        let active = live.snapshot();
        assert_eq!(
            active
                .effective
                .environment("BP_WEB_SNAPSHOT_TEST_KEY")
                .as_deref(),
            Some(secret)
        );
        assert_eq!(
            active.settings.providers["fixture"].base_url.as_deref(),
            Some("https://example.invalid/v2")
        );
        assert_eq!(
            codex_auth_file(&active.settings.providers["fixture"], &active.effective),
            Some(codex_home.join("auth.json"))
        );
        Ok(())
    }

    #[test]
    fn loopback_url_accepts_only_machine_local_hosts() {
        for host in [
            "localhost",
            "nested.localhost",
            "a.b.localhost",
            "127.0.0.1",
            "127.255.255.254",
            "[::1]",
        ] {
            assert!(loopback_url(
                &reqwest::Url::parse(&format!("http://{host}:8000/v1")).unwrap()
            ));
        }
        for host in [
            "localhost.example.org",
            "notlocalhost",
            "128.0.0.1",
            "192.168.1.2",
            "[::2]",
        ] {
            assert!(!loopback_url(
                &reqwest::Url::parse(&format!("http://{host}:8000/v1")).unwrap()
            ));
        }
    }

    #[test]
    fn opencode_models_mapping_is_documented_but_not_an_auth_check() {
        for kind in KINDS
            .iter()
            .copied()
            .filter(|kind| !matches!(*kind, "codex" | "scripted"))
        {
            for (base, expected) in [
                (
                    "https://opencode.ai/zen/go/v1",
                    "https://opencode.ai/inference/go/v1/models",
                ),
                (
                    "https://opencode.ai/zen/v1",
                    "https://opencode.ai/inference/v1/models",
                ),
                (
                    "https://opencode.ai/zen/go/v1/messages",
                    "https://opencode.ai/inference/go/v1/models",
                ),
                (
                    "https://opencode.ai:444/zen/go/v1",
                    "https://opencode.ai/inference/go/v1/models",
                ),
            ] {
                let base = reqwest::Url::parse(base).unwrap();
                let confirmed = confirmed_host(kind, &base);
                assert_eq!(confirmed, Some(ConfirmedHost::OpenCode));
                assert_eq!(model_url(&base, confirmed).unwrap().as_str(), expected);
            }
        }
        for base in [
            "https://opencode.ai.example.org/zen/go/v1",
            "http://opencode.ai/zen/go/v1",
        ] {
            assert_eq!(
                confirmed_host("chat-completions", &reqwest::Url::parse(base).unwrap()),
                None
            );
        }
        for code in 100..=599 {
            let status = reqwest::StatusCode::from_u16(code).unwrap();
            let expected = if matches!(code, 401 | 403) {
                ("failed", "endpoint rejected authentication")
            } else {
                (
                    "unchecked",
                    "models list documented; auth not confirmed for this endpoint",
                )
            };
            assert_eq!(auth_status(Some(ConfirmedHost::OpenCode), status), expected);
        }
        for confirmed in [
            ConfirmedHost::OpenAi,
            ConfirmedHost::Anthropic,
            ConfirmedHost::OpenRouter,
        ] {
            assert_eq!(
                auth_status(Some(confirmed), reqwest::StatusCode::OK).0,
                "ok"
            );
        }
        assert_eq!(auth_status(None, reqwest::StatusCode::OK).0, "unchecked");
    }

    #[test]
    fn confirmed_host_table_is_exact() {
        for kind in [
            "responses",
            "chat-completions",
            "decisions",
            "openai-decisions",
        ] {
            assert_eq!(
                confirmed_host(
                    kind,
                    &reqwest::Url::parse("https://api.openai.com").unwrap()
                ),
                Some(ConfirmedHost::OpenAi)
            );
            assert_eq!(
                model_url(
                    &reqwest::Url::parse("https://api.openai.com").unwrap(),
                    Some(ConfirmedHost::OpenAi)
                )
                .unwrap()
                .as_str(),
                "https://api.openai.com/v1/models"
            );
        }
        assert_eq!(
            confirmed_host(
                "anthropic-messages",
                &reqwest::Url::parse("https://api.anthropic.com/v1").unwrap()
            ),
            Some(ConfirmedHost::Anthropic)
        );
        assert_eq!(
            confirmed_host(
                "openrouter-decisions",
                &reqwest::Url::parse("https://openrouter.ai/api/alpha/decisions").unwrap()
            ),
            Some(ConfirmedHost::OpenRouter)
        );
        for base in [
            "https://api.openai.com.example.org/v1",
            "http://api.openai.com/v1",
            "https://api.openai.com:444/v1",
            "http://127.0.0.1:8000/v1",
        ] {
            assert_eq!(
                confirmed_host("responses", &reqwest::Url::parse(base).unwrap()),
                None
            );
        }
        assert_eq!(
            confirmed_host(
                "anthropic-messages",
                &reqwest::Url::parse("https://api.openai.com/v1").unwrap()
            ),
            None
        );
        assert_eq!(
            model_url(
                &reqwest::Url::parse("https://api.anthropic.com/v1/").unwrap(),
                Some(ConfirmedHost::Anthropic)
            )
            .unwrap()
            .as_str(),
            "https://api.anthropic.com/v1/models?limit=1000"
        );
    }
}
