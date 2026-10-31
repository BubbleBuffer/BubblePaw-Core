use super::*;
use crate::config::Override;
use bubblepaw_core::{ProviderError, ProviderErrorKind, ToolDefinition};
use serde_json::json;
use std::path::Path;

fn scripted_config(workspace: &Path, provider: &str, model: &str) -> EffectiveConfig {
    let providers: toml::Value =
        toml::from_str(&format!("[{provider}]\nkind='scripted'\n")).unwrap();
    EffectiveConfig::load(LoadOptions {
        workspace: workspace.to_owned(),
        home: None,
        custom_file: None,
        env: Vec::new(),
        cli: vec![
            Override::replace("providers", providers),
            Override::set(&format!("model.main={model}")).unwrap(),
            Override::set(&format!("model.provider={provider}")).unwrap(),
            Override::set("approvals.read_only=true").unwrap(),
        ],
    })
    .unwrap()
}

fn scripted_request(model: &str) -> ModelRequest {
    ModelRequest {
        model: model.to_owned(),
        tools: vec![ToolDefinition {
            name: "js".into(),
            description: "Synthetic code surface; calls are never executed".into(),
            input_schema: json!({"type":"object"}),
            read_only: true,
            approval: Default::default(),
        }],
        ..Default::default()
    }
}

#[tokio::test]
async fn retained_request_binding_keeps_its_model_and_provider_generation() {
    let directory = tempfile::tempdir().unwrap();
    let initial = scripted_config(directory.path(), "demo", "first");
    let live = Arc::new(LiveProviderConfig::new(initial).unwrap());
    let (old_model, old_provider) = live.request_binding();
    assert_eq!(old_model, "demo/first");

    let next = scripted_config(directory.path(), "replacement", "second");
    {
        let _update = live.lock_update().unwrap();
        live.apply_prepared(live.prepare(next).unwrap());
    }
    let (new_model, new_provider) = live.request_binding();
    assert_eq!(new_model, "replacement/second");
    assert_eq!(old_provider.models()[0].id, "demo/scripted");
    assert_eq!(new_provider.models()[0].id, "replacement/scripted");

    // Context preparation may finish after the old route is removed. The
    // retained binding still dispatches using its own matching registry.
    let reply = old_provider
        .generate(
            scripted_request(&old_model),
            Arc::new(|_| {}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(reply.tool_calls.len(), 1);
    let error = live
        .generate(
            scripted_request(&old_model),
            Arc::new(|_| {}),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        ProviderError::of(&error).map(|error| &error.kind),
        Some(&ProviderErrorKind::Fatal)
    );
    let reply = new_provider
        .generate(
            scripted_request(&new_model),
            Arc::new(|_| {}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(reply.tool_calls.len(), 1);
}

#[tokio::test]
async fn account_reload_preserves_the_inflight_generation_attribution() {
    let directory = tempfile::tempdir().unwrap();
    let mut options = scripted_config(directory.path(), "demo", "model").load_options();
    options
        .cli
        .push(Override::set("providers.demo.account=original").unwrap());
    let live =
        Arc::new(LiveProviderConfig::new(EffectiveConfig::load(options.clone()).unwrap()).unwrap());
    let (model, old) = live.request_binding();
    options
        .cli
        .push(Override::set("providers.demo.account=replacement").unwrap());
    {
        let _update = live.lock_update().unwrap();
        live.apply_prepared(
            live.prepare(EffectiveConfig::load(options).unwrap())
                .unwrap(),
        );
    }
    let (_, new) = live.request_binding();
    let (result, rows) = bubblepaw_core::capture_request_usage(
        "session",
        None,
        bubblepaw_core::RequestKind::Main,
        async {
            old.generate(
                scripted_request(&model),
                Arc::new(|_| {}),
                CancellationToken::new(),
            )
            .await?;
            new.generate(
                scripted_request(&model),
                Arc::new(|_| {}),
                CancellationToken::new(),
            )
            .await?;
            Ok::<_, anyhow::Error>(())
        },
    )
    .await;
    result.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].account, "original");
    assert_eq!(rows[1].account, "replacement");
    assert!(rows.iter().all(|row| row.provider == "demo"));
}

#[tokio::test]
async fn initial_vendor_namespace_uses_default_and_canonical_role_references() {
    let directory = tempfile::tempdir().unwrap();
    let config = scripted_config(directory.path(), "demo", "vendor/model");
    let live = Arc::new(LiveProviderConfig::new(config).unwrap());
    let (model, provider) = live.request_binding();
    assert_eq!(model, "demo/vendor/model");
    assert!(
        live.snapshot()
            .settings
            .roles
            .summary
            .models
            .contains(&model)
    );
    // No built-in decider: it stays empty until configured.
    assert!(live.snapshot().settings.roles.decider.models.is_empty());
    provider
        .generate(
            scripted_request(&model),
            Arc::new(|_| {}),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let next = scripted_config(directory.path(), "replacement", "vendor/next");
    {
        let _update = live.lock_update().unwrap();
        live.apply_prepared(live.prepare(next).unwrap());
    }
    let (model, provider) = live.request_binding();
    assert_eq!(model, "replacement/vendor/next");
    assert!(
        live.snapshot()
            .settings
            .roles
            .summary
            .models
            .contains(&model)
    );
    provider
        .generate(
            scripted_request(&model),
            Arc::new(|_| {}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let error = live
        .generate(
            scripted_request("vendor/next"),
            Arc::new(|_| {}),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        ProviderError::of(&error).map(|error| &error.kind),
        Some(&ProviderErrorKind::Fatal)
    );
}

#[test]
fn removed_main_route_cannot_become_a_vendor_namespace_on_later_reload() {
    let directory = tempfile::tempdir().unwrap();
    let initial = scripted_config(directory.path(), "demo", "first");
    let live = LiveProviderConfig::new(initial).unwrap();
    {
        let _update = live.lock_update().unwrap();
        live.apply_prepared(
            live.prepare(scripted_config(directory.path(), "replacement", "second"))
                .unwrap(),
        );
    }
    let removed_main = scripted_config(directory.path(), "replacement", "demo/first");
    let serving = live.snapshot();
    let error = live
        .prepare(removed_main.clone())
        .err()
        .expect("removed route must reject");
    assert_eq!(
        ProviderError::of(&error).map(|error| &error.kind),
        Some(&ProviderErrorKind::Fatal)
    );
    assert!(error.to_string().contains("route demo"));
    assert!(Arc::ptr_eq(&serving, &live.snapshot()));
    {
        let _update = live.lock_update().unwrap();
        live.apply_prepared(
            live.prepare(scripted_config(directory.path(), "replacement", "third"))
                .unwrap(),
        );
    }
    assert!(live.prepare(removed_main).is_err());
    assert!(
        live.prepare(scripted_config(directory.path(), "demo", "first"))
            .is_ok()
    );
}
