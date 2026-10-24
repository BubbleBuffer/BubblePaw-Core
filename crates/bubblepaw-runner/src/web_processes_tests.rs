//! HTTP handlers for owned command jobs: listing and user backgrounding.
use super::*;
use crate::{config::ExecConfig, tools::WorkspaceTools};
use bubblepaw_core::{HarnessConfig, ToolRegistry};

fn state_with(services: WebServices) -> Arc<WebState> {
    let store = Store::in_memory().unwrap();
    let harness = Arc::new(Harness::new(
        store.clone(),
        Arc::new(crate::provider::ScriptedProvider::new("fixture")),
        ToolRegistry::new(),
        HarnessConfig::default(),
    ));
    let (events, _) = broadcast::channel(4);
    let control = SessionControl::new(harness.clone(), store.clone(), "fixture");
    Arc::new(WebState {
        harness,
        store,
        config: WebConfig {
            workspace: "fixture".into(),
            model: "fixture".into(),
            provider: "demo".into(),
            read_only: false,
        },
        port: 8788,
        serving: Mutex::new(ServingView::new().unwrap()),
        transient: Mutex::default(),
        events,
        refresh: Arc::new(Notify::new()),
        stop: CancellationToken::new(),
        services,
        control,
        wake: None,
        wake_host: Mutex::new(None),
    })
}

fn process_services(root: &std::path::Path) -> WebServices {
    let processes = ProcessTools::with_config(
        WorkspaceTools::new(root).unwrap(),
        ExecConfig::default(),
        root.join("data"),
    )
    .unwrap();
    WebServices {
        processes: Some(Arc::new(processes)),
        ..WebServices::default()
    }
}

#[tokio::test]
async fn owned_session_lists_no_jobs_and_unknown_sessions_are_not_found() {
    let root = tempfile::tempdir().unwrap();
    let state = state_with(process_services(root.path()));
    let id = state.store.create_session("owned").unwrap().id;
    let Json(listing) = list_processes(State(state.clone()), Path(id))
        .await
        .ok()
        .unwrap();
    assert_eq!(listing, json!({"jobs": []}));
    let missing = list_processes(State(state), Path("missing".into()))
        .await
        .err()
        .unwrap();
    assert_eq!(missing.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn background_rejects_malformed_and_unowned_handles() {
    let root = tempfile::tempdir().unwrap();
    let state = state_with(process_services(root.path()));
    let id = state.store.create_session("owned").unwrap().id;
    // A handle this session does not own is reported as unknown, never acted on.
    for (handle, status) in [
        ("bogus", StatusCode::BAD_REQUEST),
        (
            "process:00000000-0000-0000-0000-000000000000",
            StatusCode::NOT_FOUND,
        ),
    ] {
        let error = background_process(State(state.clone()), Path((id.clone(), handle.to_owned())))
            .await
            .err()
            .unwrap();
        assert_eq!(error.0, status, "{handle}");
    }
    let missing_session = background_process(
        State(state),
        Path(("missing".into(), "process:not-started".into())),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(missing_session.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn processes_are_unavailable_without_the_service_and_listing_stays_empty() {
    let state = state_with(WebServices::default());
    let id = state.store.create_session("bare").unwrap().id;
    let Json(listing) = list_processes(State(state.clone()), Path(id.clone()))
        .await
        .ok()
        .unwrap();
    assert_eq!(listing, json!({"jobs": []}));
    let error = background_process(State(state), Path((id, "process:none".into())))
        .await
        .err()
        .unwrap();
    assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(error.1.contains("Processes are unavailable"));
}
