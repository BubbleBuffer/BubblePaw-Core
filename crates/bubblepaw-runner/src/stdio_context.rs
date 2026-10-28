//! Thin stdio dispatch into the shared read-only context service.
use anyhow::{Result, bail};
use bubblepaw_core::Store;
use serde_json::Value;

pub fn command<'a>(
    store: &Store,
    method: &str,
    params: &Value,
    session_id: impl FnOnce() -> Result<&'a str>,
) -> Result<Value> {
    match method {
        "context.view" | "context.structure" | "context.summary" | "context.events"
        | "history.range" => {
            bubblepaw_runner::context_reads::read(store, session_id()?, method, params)
        }
        _ => bail!("unknown method: {method}"),
    }
}
