//! One-shot CLI commands over the store. They never start a provider or MCP
//! server; each operation is a call into the shared host_api table.
use crate::cli::{CatalogCommand, Command, ServingCommand, TranscriptFormat};
use crate::print_json;
use anyhow::Result;
use bubblepaw_core::{ImportFormat, Store};
use bubblepaw_runner::{
    host_api::{HostCtx, Profile, ReadScope, dispatch},
    tools::{ProcessTools, WorkspaceTools},
    usage::UsageSettings,
    wake::WakeCoordinator,
};
use serde_json::{Value, json};
use std::{io::Write, path::Path, sync::Arc, time::Duration};

/// Messages per `session.history` page; each page is written before the next is read.
const HISTORY_PAGE: usize = 1000;

pub(crate) async fn run(
    command: &Command,
    store: Store,
    workspace: &Path,
    usage: UsageSettings,
) -> Result<()> {
    let mut ctx = HostCtx::for_store(store.clone(), workspace);
    ctx.usage = usage;
    // No host owns the store on this path, so the wake coordinator has no driver.
    // It exists so session.delete suppresses wake and cancels this store's jobs.
    let wake = Arc::new(WakeCoordinator::new(false, Duration::ZERO));
    let processes = Arc::new(ProcessTools::new(WorkspaceTools::new(workspace)?));
    link_processes(&processes, &wake, &store);
    ctx.wake = Some(wake);
    ctx.processes = Some(processes);
    run_with_ctx(command, ctx).await
}

/// Connects the one-shot processes to the coordinator and store that session.delete uses.
fn link_processes(processes: &ProcessTools, wake: &Arc<WakeCoordinator>, store: &Store) {
    processes.set_wake(wake.clone());
    processes.set_store(store.clone());
}

/// Runs one command against a wired context; tests inject live services here.
async fn run_with_ctx(command: &Command, ctx: HostCtx) -> Result<()> {
    let store = ctx.store.clone();
    match command {
        Command::Sessions => print_json(&call(&host_scope(&ctx), "session.list", json!({})).await?),
        Command::History { session } => write_history(&for_session(&ctx, session)?, session).await,
        Command::Stats => {
            let mut stats = call(&ctx, "storage.stats", json!({})).await?;
            stats["sessions"] = call(&host_scope(&ctx), "session.list", json!({})).await?;
            print_json(&stats)
        }
        Command::Import {
            format,
            file,
            title,
            children,
        } => {
            let format = match format {
                TranscriptFormat::ClaudeCode => ImportFormat::Claude,
                TranscriptFormat::Codex => ImportFormat::Codex,
            };
            let params =
                json!({"path": file, "format": format, "title": title, "children": children});
            print_json(&call(&ctx, "session.import", params).await?)
        }
        Command::Archive { chat } => {
            let scoped = for_session(&ctx, chat)?;
            print_json(&call(&scoped, "session.archive", json!({"session_id": chat})).await?)
        }
        Command::Unarchive { chat } => {
            let scoped = for_session(&ctx, chat)?;
            print_json(&call(&scoped, "session.unarchive", json!({"session_id": chat})).await?)
        }
        Command::Search { query, chat } => {
            let scoped = match chat {
                Some(chat) => for_session(&ctx, chat)?,
                None => host_scope(&ctx),
            };
            let params = json!({"query": query, "session_id": chat, "limit": 50});
            print_json(&call(&scoped, "session.search", params).await?)
        }
        // Catalog and serving maintenance have no table method yet; they stay direct.
        Command::Catalog {
            command: CatalogCommand::Rebuild,
        } => print_json(&json!({"sessions": store.catalog_rebuild()?})),
        Command::Serving {
            command: ServingCommand::Wipe,
        } => {
            store.serving_wipe()?;
            print_json(&json!({"wiped": true}))
        }
        Command::Delete { session } => {
            let scoped = for_session(&ctx, session)?;
            print_json(&call(&scoped, "session.delete", json!({"session_id": session})).await?)
        }
        _ => unreachable!("only one-shot commands reach the store path"),
    }
}

async fn call(ctx: &HostCtx, method: &str, params: Value) -> Result<Value> {
    Ok(dispatch(ctx, Profile::Full, method, params).await?)
}

/// The context for one target session: its stored workspace binding, which is
/// what the table's binding check compares against. An unbound session keeps
/// the invocation workspace.
fn for_session(ctx: &HostCtx, session: &str) -> Result<HostCtx> {
    let mut scoped = ctx.clone();
    if let Err(error) = ctx.store.session_root_id(session) {
        if error
            .downcast_ref::<rusqlite::Error>()
            .is_some_and(|error| matches!(error, rusqlite::Error::QueryReturnedNoRows))
        {
            return Ok(scoped);
        }
        return Err(error);
    }
    if let Some(workspace) = ctx.store.session_workspace(session)? {
        scoped.workspace = workspace;
    }
    scoped.read_scope = ReadScope::Workspace(scoped.workspace.clone());
    Ok(scoped)
}

fn host_scope(ctx: &HostCtx) -> HostCtx {
    let mut ctx = ctx.clone();
    ctx.read_scope = ReadScope::Host;
    ctx
}

#[cfg(test)]
#[path = "cli_store_tests.rs"]
mod tests;

/// Writes the whole transcript as one JSON array, paging through `session.history`.
async fn write_history(ctx: &HostCtx, session: &str) -> Result<()> {
    let mut after_seq = 0;
    let mut first = true;
    std::io::stdout().lock().write_all(b"[\n")?;
    loop {
        let params = json!({"session_id": session, "after_seq": after_seq, "limit": HISTORY_PAGE});
        let page = call(ctx, "session.history", params).await?;
        let mut writer = std::io::stdout().lock();
        for message in page["messages"].as_array().into_iter().flatten() {
            if !first {
                writer.write_all(b",\n")?;
            }
            first = false;
            serde_json::to_writer(&mut writer, message)?;
        }
        writer.flush()?;
        match page["next_seq"].as_i64() {
            Some(next) => after_seq = next,
            None => break,
        }
    }
    let mut writer = std::io::stdout().lock();
    writer.write_all(b"\n]\n")?;
    writer.flush()?;
    Ok(())
}
