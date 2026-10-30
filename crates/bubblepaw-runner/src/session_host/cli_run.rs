//! One-shot CLI runs. Admission, cancellation and approval decisions go through
//! the host_api table; this adapter only presents events and waits for the result.
use anyhow::{Result, bail, ensure};
use bubblepaw_core::{CancellationToken, EventKind};
use bubblepaw_runner::{
    cli_output::Output,
    host_api::{HostCtx, Profile, dispatch, events},
};
use serde_json::json;
use std::time::Duration;
use tokio::sync::broadcast;

/// What a one-shot request asks the host to start in its session.
pub(super) enum OneShot<'a> {
    Prompt(&'a str),
    Script { label: &'a str, body: String },
    Resume,
}

pub(super) async fn run_session(
    ctx: &HostCtx,
    session: &str,
    request: OneShot<'_>,
    json_output: bool,
    output: Output,
    cancellation: CancellationToken,
) -> Result<()> {
    let script = matches!(request, OneShot::Script { .. });
    // Subscribe before the run starts so its first events are presented.
    let mut receiver = events::subscribe(ctx, None, Some(session))?.receiver;
    output.stderr(format!("session: {session}\n"))?;
    ensure!(
        !cancellation.is_cancelled(),
        "run cancelled before admission"
    );
    let (method, params) = match request {
        OneShot::Prompt(text) => ("run.start", json!({"session_id": session, "text": text})),
        OneShot::Script { label, body } => (
            "run.start",
            json!({"session_id": session, "text": label, "script": body}),
        ),
        OneShot::Resume => ("run.resume", json!({"session_id": session})),
    };
    dispatch(ctx, Profile::Full, method, params).await?;
    let target = session.to_owned();
    let display_output = output.clone();
    let mut display = tokio::spawn(async move {
        loop {
            let event = match receiver.recv().await {
                Ok(event) => event,
                // A lagged presentation skips what it missed; the run result still reports the outcome.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            };
            if event.session_id != target {
                continue;
            }
            display_output.event(&event, json_output)?;
            if matches!(event.kind, EventKind::SessionSettled {}) {
                break;
            }
        }
        Ok::<_, anyhow::Error>(())
    });
    let run = dispatch(
        ctx,
        Profile::Full,
        "run.wait",
        json!({"session_id": session}),
    );
    tokio::pin!(run);
    let result = tokio::select! {
        biased;
        result = &mut run => result,
        _ = cancellation.cancelled() => {
            let _ = dispatch(ctx, Profile::Full, "run.cancel", json!({"session_id": session})).await;
            run.await
        }
    }?;
    match tokio::time::timeout(Duration::from_secs(2), &mut display).await {
        Ok(task) => task??,
        Err(_) => {
            display.abort();
            let _ = display.await;
            output.stderr("presentation ended before all events were displayed\n")?;
        }
    }
    let status = result["status"].as_str().unwrap_or_default();
    if json_output {
        output.json(&json!({"type":"result","session_id":session,"run":result}))?;
    } else if script && status == "completed" {
        output.stdout(format!("{}\n", result["text"].as_str().unwrap_or_default()))?;
    }
    if status != "completed" {
        bail!(
            "run {}: {}",
            status,
            result["error"].as_str().unwrap_or_default()
        );
    }
    Ok(())
}

#[cfg(test)]
#[path = "cli_run_tests.rs"]
mod tests;
