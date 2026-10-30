//! Same-store parity under each explicit read scope. A session bound to another
//! workspace is read through the stdio framing path and through `POST /rpc`, under
//! one scope on both. Every read is compared exactly, as in `same_store.rs`, and
//! the allow or deny outcome for the foreign session is asserted by name. Host
//! scope allows it; workspace scope denies it on both transports.
use super::same_store::{
    Exchanged, assert_seeded, direct_ctx, exchange_all, frames, new_harness, reply, seed,
    seed_session, spawn_web,
};
use anyhow::Result;
use bubblepaw_core::Store;
use bubblepaw_runner::host_api::ReadScope;
use serde_json::json;

/// Reads that name one session. Each must allow or deny the foreign session as a whole.
const SESSION_READS: &[&str] = &["session.state", "session.history", "session.view"];
const FOREIGN_TITLE: &str = "parity same store foreign";

/// Every read on the own and the foreign session, under one scope.
struct Scoped {
    own: String,
    foreign: String,
    own_replies: Vec<Exchanged>,
    foreign_replies: Vec<Exchanged>,
}

/// `workspace` binds both transports to the root workspace. Otherwise the stdio
/// side uses host scope and the web host keeps its default.
async fn run_scope(workspace: bool) -> Result<Scoped> {
    let root = tempfile::tempdir()?;
    // Kept alive until the exchanges finish, so the foreign binding stays real.
    let foreign_dir = tempfile::tempdir()?;
    let store = Store::open_data_dir(root.path().join("data"))?;
    let own = seed(&store, root.path())?;
    let foreign = seed_session(&store, foreign_dir.path(), FOREIGN_TITLE, 1)?;
    let (read_scope, web_scope) = if workspace {
        let scope = ReadScope::Workspace(root.path().to_path_buf());
        (scope.clone(), Some(scope))
    } else {
        (ReadScope::Host, None)
    };
    let harness = new_harness(&store);
    let ctx = direct_ctx(harness.clone(), store.clone(), root.path(), read_scope)?;
    let web = spawn_web(harness, store, root.path(), web_scope).await?;
    let client = reqwest::Client::new();
    let own_replies = exchange_all(&ctx, &client, &web.base, frames(&own)?).await;
    let foreign_replies = exchange_all(&ctx, &client, &web.base, frames(&foreign)?).await;
    web.stop().await?;
    assert_seeded(&own_replies);
    Ok(Scoped {
        own,
        foreign,
        own_replies,
        foreign_replies,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_scope_allows_a_foreign_session_on_both_transports() -> Result<()> {
    let run = run_scope(false).await?;
    for method in SESSION_READS {
        let answer = reply(
            &run.foreign_replies,
            method,
            json!({"session_id": run.foreign}),
        );
        assert!(
            answer.get("error").is_none() && answer.get("result").is_some(),
            "{method} denied the foreign session in host scope: {answer}"
        );
    }
    let history = reply(
        &run.foreign_replies,
        "session.history",
        json!({"session_id": run.foreign}),
    );
    assert!(
        history.to_string().contains(&format!("{FOREIGN_TITLE} 0")),
        "host scope history lacks the foreign message: {history}"
    );
    let list = reply(&run.own_replies, "session.list", json!({}));
    assert!(
        list.to_string().contains(&run.foreign),
        "host scope list omits the foreign session: {list}"
    );
    assert!(
        list.to_string().contains(&run.own),
        "host scope list omits the own session: {list}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_scope_denies_a_foreign_session_on_both_transports() -> Result<()> {
    let run = run_scope(true).await?;
    for method in SESSION_READS {
        let answer = reply(
            &run.foreign_replies,
            method,
            json!({"session_id": run.foreign}),
        );
        let message = answer["error"]["message"].as_str().unwrap_or_default();
        assert!(
            answer.get("result").is_none() && message.contains("another workspace"),
            "{method} did not deny the foreign session in workspace scope: {answer}"
        );
    }
    let list = reply(&run.own_replies, "session.list", json!({}));
    assert!(
        !list.to_string().contains(&run.foreign),
        "workspace scope list shows the foreign session: {list}"
    );
    assert!(
        list.to_string().contains(&run.own),
        "workspace scope list omits the own session: {list}"
    );
    Ok(())
}
