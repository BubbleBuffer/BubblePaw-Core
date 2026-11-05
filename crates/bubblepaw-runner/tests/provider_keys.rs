//! Synthetic provider keys only: persistence, request generations and redaction.
use anyhow::Result;
use bubblepaw_core::{CancellationToken, ModelRequest, Provider, Store};
use bubblepaw_runner::{
    config::{EffectiveConfig, LoadOptions, Override},
    host_api::{HostCtx, HostError, Profile, dispatch, rpc},
    provider::LiveProviderConfig,
    provider_setup::ProviderSetup,
};
use serde_json::{Value, json};
use std::{
    io::Write,
    sync::{Arc, Mutex},
};

const FIRST: &str = "synthetic-provider-key-marker-first";
const SECOND: &str = "synthetic-provider-key-marker-second";
const ENV: &str = "synthetic-provider-key-marker-environment";

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);
impl Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Fixture {
    root: tempfile::TempDir,
    ctx: HostCtx,
    live: Arc<LiveProviderConfig>,
}
fn fixture(base: &str, with_env: bool, with_key_env: bool) -> Result<Fixture> {
    let root = tempfile::tempdir()?;
    let config = root.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[model]\nprovider='fixture'\nmain='fixture/model'\n[providers.fixture]\nkind='chat-completions'\nbase_url='{base}'\n{}models=[{{id='model'}}]\n[providers.demo]\nkind='scripted'\nmodels=[{{id='first'}}]\n",
            if with_key_env {
                "key_env='FIXTURE_API_KEY'\n"
            } else {
                ""
            }
        ),
    )?;
    let effective = EffectiveConfig::load(LoadOptions {
        workspace: root.path().into(),
        home: None,
        custom_file: Some(config),
        env: if with_env {
            vec![("FIXTURE_API_KEY".into(), ENV.into())]
        } else {
            Vec::new()
        },
        cli: vec![Override::value(
            "storage.data_dir",
            toml::Value::String(root.path().join("data").to_string_lossy().into_owned()),
        )],
    })?;
    let live = Arc::new(LiveProviderConfig::new(effective.clone())?);
    let setup = ProviderSetup::new(effective.load_options(), &effective)?.with_live(live.clone());
    let mut ctx = HostCtx::for_store(Store::in_memory()?, root.path());
    ctx.providers = Some(setup);
    ctx.provider_config = Some(live.clone());
    Ok(Fixture { root, ctx, live })
}
async fn call(ctx: &HostCtx, method: &str, params: Value) -> Result<Value, HostError> {
    dispatch(ctx, Profile::Full, method, params).await
}
async fn request(model: String, provider: Arc<dyn Provider>) -> Result<()> {
    let response = provider
        .generate(
            ModelRequest {
                model,
                ..Default::default()
            },
            Arc::new(|_| {}),
            CancellationToken::new(),
        )
        .await?;
    assert_eq!(response.text, "ok");
    Ok(())
}

#[tokio::test]
async fn provider_key_contract_request_uptake_and_no_secret_diagnostics() -> Result<()> {
    let logs = Logs::default();
    let writer = logs.clone();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .init();
    tracing::info!("provider-key-log-capture-active");
    let headers = Arc::new(Mutex::new(Vec::new()));
    let capture = headers.clone();
    let app = axum::Router::new().route("/chat/completions", axum::routing::post(
        move |headers: axum::http::HeaderMap| {
            capture.lock().unwrap().push(headers.get("authorization").unwrap().to_str().unwrap().to_owned());
            async { ([("content-type", "text/event-stream")],
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n") }
        }
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let f = fixture(&base, true, true)?;
    let original = std::fs::read(f.root.path().join("config.toml"))?;
    assert_eq!(
        call(&f.ctx, "providers.key.get", json!({"name":"fixture"})).await?,
        json!({"source":"environment","is_set":true})
    );
    let (model, provider) = f.live.request_binding();
    request(model, provider).await?;
    for (key, expected) in [(FIRST, FIRST), (SECOND, SECOND)] {
        let reply = call(
            &f.ctx,
            "providers.key.set",
            json!({"name":"fixture","value":key}),
        )
        .await?;
        assert_eq!(
            reply,
            json!({"source":"stored","is_set":true,"applied":true,"restart_needed":false})
        );
        let (model, provider) = f.live.request_binding();
        request(model, provider.clone()).await?;
        assert_eq!(
            headers.lock().unwrap().last().unwrap(),
            &format!("Bearer {expected}")
        );
        if key == FIRST {
            // A bound request keeps its original key even after rotation.
            call(
                &f.ctx,
                "providers.key.set",
                json!({"name":"fixture","value":SECOND}),
            )
            .await?;
            request("fixture/model".into(), provider).await?;
            assert_eq!(
                headers.lock().unwrap().last().unwrap(),
                &format!("Bearer {FIRST}")
            );
        }
    }
    f.live.reload()?;
    let (model, provider) = f.live.request_binding();
    request(model, provider).await?;
    assert_eq!(
        headers.lock().unwrap().last().unwrap(),
        &format!("Bearer {SECOND}")
    );
    for method in ["providers.list", "config.get", "initialize"] {
        let reply = call(&f.ctx, method, json!({})).await?;
        for marker in [FIRST, SECOND, ENV] {
            assert!(!reply.to_string().contains(marker));
        }
    }
    let removed = call(&f.ctx, "providers.key.remove", json!({"name":"fixture"})).await?;
    assert_eq!(removed["source"], "environment");
    let (model, provider) = f.live.request_binding();
    request(model, provider).await?;
    assert_eq!(
        headers.lock().unwrap().last().unwrap(),
        &format!("Bearer {ENV}")
    );
    assert_eq!(std::fs::read(f.root.path().join("config.toml"))?, original);
    for params in [
        json!({"name":"fixture","value":format!("{FIRST}\n")}),
        json!({"name":"fixture","value": {FIRST: FIRST}}),
        json!({"name":"fixture","value":FIRST,FIRST: FIRST}),
        json!({"name":format!("../{FIRST}"),"value":FIRST}),
    ] {
        let reply = rpc::handle(
            &f.ctx,
            Profile::Full,
            json!({
                "jsonrpc":"2.0","id":1,"method":"providers.key.set","params":params
            }),
        )
        .await
        .unwrap();
        assert!(reply.get("error").is_some());
        assert!(!reply.to_string().contains(FIRST));
    }
    // The same raw parser used by stdio must never quote invalid JSON bytes.
    let malformed = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"providers.key.set\",\"params\":{{\"value\":\"{FIRST}\""
    );
    let parse_error = rpc::handle_bytes(&f.ctx, Profile::Full, malformed.as_bytes())
        .await
        .unwrap();
    assert_eq!(parse_error["error"]["code"], -32700);
    assert!(!parse_error.to_string().contains(FIRST));
    let request_debug = format!(
        "{:?}",
        bubblepaw_runner::host_api::providers::SetKeyParams {
            name: "fixture".into(),
            value: FIRST.into(),
        }
    );
    assert!(!request_debug.contains(FIRST));
    for profile in [Profile::Read, Profile::Edit] {
        for (method, params) in [
            ("providers.key.set", json!({"name":"fixture","value":FIRST})),
            ("providers.key.remove", json!({"name":"fixture"})),
        ] {
            let error = dispatch(&f.ctx, profile, method, params).await.unwrap_err();
            assert_eq!(error.rpc_code(), -32003);
            assert!(!format!("{error:?}").contains(FIRST));
        }
    }
    for method in ["providers.list", "config.get", "initialize"] {
        let reply = call(&f.ctx, method, json!({})).await?;
        for marker in [FIRST, SECOND, ENV] {
            assert!(!reply.to_string().contains(marker));
        }
    }
    // A provider with no environment reference can be configured before its key.
    let empty = fixture("https://example.invalid/v1", false, false)?;
    assert!(empty.live.snapshot().unavailable().is_some());
    assert_eq!(
        call(&empty.ctx, "providers.key.get", json!({"name":"fixture"})).await?["source"],
        "none"
    );
    call(
        &empty.ctx,
        "providers.key.set",
        json!({"name":"fixture","value":FIRST}),
    )
    .await?;
    assert!(empty.live.snapshot().unavailable().is_none());
    call(
        &empty.ctx,
        "providers.key.remove",
        json!({"name":"fixture"}),
    )
    .await?;
    assert!(empty.live.snapshot().unavailable().is_some());
    call(
        &empty.ctx,
        "providers.key.set",
        json!({"name":"fixture","value":FIRST}),
    )
    .await?;
    call(&empty.ctx, "roles.set_main", json!({"model":"demo/first"})).await?;
    call(&empty.ctx, "providers.remove", json!({"name":"fixture"})).await?;
    assert_eq!(
        std::fs::read_dir(empty.root.path().join("data/secrets"))?.count(),
        0
    );
    assert_eq!(
        call(&empty.ctx, "providers.key.get", json!({"name":"fixture"}))
            .await
            .unwrap_err()
            .http_status(),
        404
    );
    assert_eq!(
        call(
            &empty.ctx,
            "providers.key.set",
            json!({"name":"demo","value":FIRST})
        )
        .await
        .unwrap_err()
        .http_status(),
        400
    );
    server.abort();
    let log_text = String::from_utf8(logs.0.lock().unwrap().clone())?;
    assert!(log_text.contains("provider-key-log-capture-active"));
    for marker in [FIRST, SECOND, ENV] {
        assert!(!log_text.contains(marker));
    }
    Ok(())
}
