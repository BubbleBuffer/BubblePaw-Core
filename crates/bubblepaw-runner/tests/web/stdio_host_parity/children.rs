use super::*;
use bubblepaw_core::{ContextStrategy, Store};

#[tokio::test]
async fn child_cancel_checks_direct_parent_workspace_and_preserves_parent() {
    let root = fixture();
    let foreign = tempfile::tempdir().unwrap();
    let foreign_child = {
        let store = Store::open_data_dir(root.path().join("data")).unwrap();
        let parent = store
            .create_session_with_workspace("foreign", foreign.path())
            .unwrap();
        store
            .create_session_with_parent("foreign child", ContextStrategy::Lcm, Some(&parent.id))
            .unwrap()
            .id
    };
    let mut host = Host::start(root.path(), &[]);
    let parent = host.open(json!({"title":"parent"})).await;
    let child = host.open(json!({"parent_session_id":parent})).await;
    let grandchild = host.open(json!({"parent_session_id":child})).await;
    let other = host.open(json!({"title":"unrelated"})).await;
    for params in [
        json!({}),
        json!({"parent":parent}),
        json!({"parent":other,"child":child}),
        json!({"parent":parent,"child":grandchild}),
        json!({"parent":parent,"child":"missing-child"}),
        json!({"parent":parent,"child":foreign_child}),
    ] {
        assert!(
            host.call("child.cancel", params)
                .await
                .get("error")
                .is_some()
        );
    }
    let cancelled = host
        .ok("child.cancel", json!({"parent":parent,"child":child}))
        .await;
    assert_eq!(cancelled["sessionId"], child);
    assert_eq!(cancelled["status"], "cancelled");
    // Reattaching the cancelled descendant observes the existing subtree marker.
    let descendant = host
        .ok("child.cancel", json!({"parent":child,"child":grandchild}))
        .await;
    assert_eq!(descendant["status"], "cancelled");
    let state = host.ok("session.state", json!({"session_id":parent})).await;
    assert!(state["runs"].as_array().unwrap().is_empty());
    assert!(state["messages"].as_array().unwrap().is_empty());
    host.stop().await;
}

#[tokio::test]
async fn child_cancel_stops_running_child_and_grandchild_and_keeps_parent_usable() {
    let root = fixture();
    let extensions = root.path().join("extensions.json");
    std::fs::write(
        &extensions,
        serde_json::to_vec(&json!([{
            "id":"fake",
            "command":fixture_binary::resolve(
                env!("CARGO_BIN_EXE_bubblepaw-fake-extension"),
                "bubblepaw-fake-extension",
            ),
            "args":[],
            "cwd":root.path(),
        }]))
        .unwrap(),
    )
    .unwrap();
    let mut host = Host::start(
        root.path(),
        &[
            "--model",
            "fake/code-model",
            "--set",
            "agent.execution_mode='js'",
            "--extensions",
            extensions.to_str().unwrap(),
        ],
    );
    let parent = host.open(json!({"title":"running subtree"})).await;
    let child_script =
        "js await tools.agent({profile:'read-only',task:'hang'}); await wait({ms:600000});";
    let root_script = format!(
        "text(await tools.agent({{profile:'read-only',task:{}}}));",
        serde_json::to_string(child_script).unwrap(),
    );
    let parent_run = host
        .ok(
            "run.start",
            json!({
                "session_id":parent,"text":"Start cancellable children","script":root_script,
            }),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (child, grandchild) = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let sessions = host.ok("session.list", json!({})).await;
            let sessions = sessions.as_array().unwrap();
            if let Some(child) = sessions
                .iter()
                .find(|session| session["parent_session_id"] == parent)
            {
                let child = child["id"].as_str().unwrap().to_owned();
                if let Some(grandchild) = sessions
                    .iter()
                    .find(|session| session["parent_session_id"] == child)
                {
                    let grandchild = grandchild["id"].as_str().unwrap().to_owned();
                    let parent_state = host.ok("session.state", json!({"session_id":parent})).await;
                    let child_state = host.ok("session.state", json!({"session_id":child})).await;
                    let grandchild_state = host
                        .ok("session.state", json!({"session_id":grandchild}))
                        .await;
                    if parent_state["runs"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|run| run["id"] == parent_run && run["status"] == "completed")
                        && child_state["runs"]
                            .as_array()
                            .unwrap()
                            .last()
                            .is_some_and(|run| run["status"] == "running")
                        && grandchild_state["runs"]
                            .as_array()
                            .unwrap()
                            .last()
                            .is_some_and(|run| run["status"] == "running")
                    {
                        break (child, grandchild);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("fixture did not establish a running child and grandchild");
    let cancelled = host
        .ok("child.cancel", json!({"parent":parent,"child":child}))
        .await;
    assert_eq!(cancelled["sessionId"], child);
    assert_eq!(cancelled["status"], "cancelled");
    for session in [&child, &grandchild] {
        let state = host
            .ok("session.state", json!({"session_id":session}))
            .await;
        assert_eq!(
            state["runs"].as_array().unwrap().last().unwrap()["status"],
            "cancelled"
        );
        // The child is interrupted inside its model-issued JS call. Core keeps
        // that unfinished outer call as unknown even after owned resources stop;
        // it must never claim that the interrupted cell completed successfully.
        // The grandchild is waiting in the provider and has no tool boundary.
        let effects = state["pending_effects"].as_array().unwrap();
        if session == &child {
            let run = state["runs"].as_array().unwrap().last().unwrap();
            assert!(
                effects.len() <= 1 && effects
                    .iter()
                    .all(|effect| effect["state"] == "unknown" && effect["run_id"] == run["id"]),
                "unexpected unresolved effect after child cleanup: {effects:?}",
            );
        } else {
            assert!(effects.is_empty(), "grandchild effects: {effects:?}");
        }
    }
    let state = host.ok("session.state", json!({"session_id":parent})).await;
    assert_eq!(
        state["runs"].as_array().unwrap().last().unwrap()["status"],
        "completed"
    );
    let next = host
        .ok("run.start", json!({"session_id":parent,"text":"probe"}))
        .await;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let state = host.ok("session.state", json!({"session_id":parent})).await;
            if let Some(run) = state["runs"]
                .as_array()
                .unwrap()
                .iter()
                .find(|run| run["id"] == next["id"])
                && run["status"] != "running"
            {
                assert_eq!(run["status"], "completed", "{run}");
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    host.stop().await;
}
