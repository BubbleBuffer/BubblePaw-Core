#![cfg(feature = "web")]
//! The HTTP routes use the same context reads as stdio and SDK callers.
#[path = "context_stdio/fixture.rs"]
mod fixture;

use anyhow::Result;
use bubblepaw_core::{ContextStrategy, Harness, HarnessConfig, Store, ToolRegistry};
use bubblepaw_runner::{
    context_reads,
    host_api::ReadScope,
    provider::ScriptedProvider,
    web::{WebConfig, WebServices, serve_with_services},
};
use fixture::Fixture;
use serde_json::{Value, json};
use std::sync::Arc;

#[tokio::test]
async fn context_get_routes_match_shared_reads_and_enforce_range_and_workspace_rules() -> Result<()>
{
    check_context_routes(true).await
}

#[tokio::test]
async fn context_get_routes_allow_other_projects_with_default_host_scope() -> Result<()> {
    check_context_routes(false).await
}

async fn check_context_routes(workspace_scope: bool) -> Result<()> {
    let f = Fixture::new().await?;
    let harness = Arc::new(Harness::new(
        f.store.clone(),
        Arc::new(ScriptedProvider::new(
            "No context read should dispatch this provider",
        )),
        ToolRegistry::default(),
        HarnessConfig::default(),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let (stop, receive) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(serve_with_services(
        listener,
        harness,
        f.store.clone(),
        WebConfig {
            workspace: f.root.path().display().to_string(),
            model: "scripted".into(),
            provider: "demo".into(),
            read_only: false,
        },
        WebServices {
            read_scope: workspace_scope.then(|| ReadScope::Workspace(f.root.path().into())),
            ..Default::default()
        },
        async {
            let _ = receive.await;
        },
    ));
    let client = reqwest::Client::new();
    let prefix = format!("{base}/api/sessions/{}", f.session);
    for resource in ["view", "structure", "events"] {
        let actual: Value = client
            .get(format!("{prefix}/context/{resource}"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        assert_eq!(
            actual,
            context_reads::read(
                &f.store,
                &f.session,
                &format!("context.{resource}"),
                &json!({})
            )?
        );
    }
    for (kind, id, expected) in [
        ("episode", &f.episode.id, Some(f.episode.summary.as_str())),
        (
            "chapter",
            &f.closed_chapter.id,
            f.closed_chapter.summary.as_deref(),
        ),
        ("chapter", &f.chapter.id, None),
    ] {
        let result: Value = client
            .get(format!("{prefix}/context/summary"))
            .query(&[("kind", kind), ("id", id.as_str())])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        assert_eq!(result["summary"], json!(expected));
    }
    let page: Value = client
        .get(format!("{prefix}/history/range"))
        .query(&[
            ("first_message_id", f.history[0].id.as_str()),
            ("last_message_id", f.history[5].id.as_str()),
            ("limit", "2"),
        ])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(page["messages"].as_array().unwrap().len(), 2);
    assert_eq!(page["messages"][0]["seq"], f.seq[0]);
    assert_eq!(
        page["messages"][1]["message"],
        serde_json::to_value(&f.history[1])?
    );
    assert_eq!(page["messages"][1]["tokens"], 42);
    assert_eq!(page["messages"][1]["source"], "reported");
    assert_eq!(page["next_seq"], f.seq[2]);
    for query in [
        "first_seq=0&last_seq=10&limit=1001",
        "first_seq=9&last_seq=1",
        "first_message_id=missing&last_message_id=missing",
    ] {
        assert_eq!(
            client
                .get(format!("{prefix}/history/range?{query}"))
                .send()
                .await?
                .status(),
            reqwest::StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        client
            .get(format!("{prefix}/context/summary?kind=chapter&id=missing"))
            .send()
            .await?
            .status(),
        reqwest::StatusCode::NOT_FOUND
    );
    let foreign_dir = tempfile::tempdir()?;
    let foreign = f.store.create_session_with_strategy_and_workspace(
        "Other workspace",
        ContextStrategy::Lcm,
        foreign_dir.path(),
    )?;
    for suffix in [
        "context/view",
        "context/structure",
        "context/events",
        "context/summary?kind=episode&id=missing",
        "history/range?first_seq=0&last_seq=10",
    ] {
        assert_eq!(
            client
                .get(format!("{base}/api/sessions/{}/{suffix}", foreign.id))
                .send()
                .await?
                .status(),
            if workspace_scope {
                reqwest::StatusCode::CONFLICT
            } else if suffix.starts_with("context/summary") {
                reqwest::StatusCode::NOT_FOUND
            } else {
                reqwest::StatusCode::OK
            }
        );
    }
    assert_eq!(f.store.messages(&f.session)?, f.history);
    assert_eq!(
        serde_json::to_value(f.store.context_view(&f.session)?)?,
        json!(Some(f.view))
    );
    let _ = stop.send(());
    task.await??;
    Ok(())
}
