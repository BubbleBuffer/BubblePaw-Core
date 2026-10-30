//! Stdio transport: framing, the event forwarder and shutdown. Every request is
//! answered by the shared host_api table through `rpc::handle_bytes`.
use anyhow::{Context, Result, ensure};
use bubblepaw_core::{CancellationToken, HarnessEvent};
use bubblepaw_runner::host_api::{HostCtx, Profile, ReadScope, events, rpc};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::mpsc;

pub(crate) async fn run(mut ctx: HostCtx, cancellation: CancellationToken) -> Result<()> {
    ctx.read_scope = ReadScope::Workspace(ctx.workspace.clone());
    let harness = ctx.harness.clone();
    let wake = ctx
        .wake_coordinator()
        .context("stdio needs the wake coordinator")?;
    let questions = ctx
        .questions
        .as_ref()
        .context("stdio needs question tools")?;
    questions.set_child_answer_harness(harness.clone())?;
    let wake_registration = wake.register_harness_scoped(harness.clone(), ctx.scope.clone());
    let wake_driver = wake.start_driver();
    let (output, mut records) = mpsc::channel::<Value>(128);
    // Console/anonymous-pipe IO can block forever on Windows. Dedicated OS
    // threads keep it out of Tokio's blocking pool so shutdown remains bounded.
    let writer = std::thread::spawn(move || {
        use std::io::Write;
        let mut stdout = std::io::stdout().lock();
        while let Some(record) = records.blocking_recv() {
            serde_json::to_writer(&mut stdout, &record)?;
            writeln!(stdout)?;
            stdout.flush()?;
        }
        Ok::<_, anyhow::Error>(())
    });
    let subscription = events::subscribe(&ctx, None, None)?;
    let mut receiver = subscription.receiver;
    let notifications = output.clone();
    // The last durable sequence forwarded; a gap is recovered with events.after from it.
    let mut forwarded = subscription.last_seq;
    let forward_ctx = ctx.clone();
    let event_forwarder = tokio::spawn(async move {
        let mut skipped = 0u64;
        loop {
            let event: HarnessEvent = match receiver.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                    skipped += count;
                    continue;
                }
                Err(_) => break,
            };
            if !events::visible(&forward_ctx, &event, None) {
                if event.durable && skipped == 0 {
                    forwarded = forwarded.max(event.sequence);
                }
                continue;
            }
            if skipped > 0 {
                if notifications.try_send(json!({"jsonrpc":"2.0","method":"events.gap","params":{"skipped":skipped,"after":forwarded,"recover_with":"events.after"}})).is_err() { skipped += 1; continue; }
                skipped = 0;
            }
            let (durable, sequence) = (event.durable, event.sequence);
            if notifications
                .try_send(json!({"jsonrpc":"2.0","method":"event","params":event}))
                .is_err()
            {
                skipped += 1;
            } else if durable {
                forwarded = forwarded.max(sequence);
            }
        }
    });
    let (input, mut incoming) = mpsc::channel(16);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        loop {
            let record = read_record(&mut stdin);
            let terminal = !matches!(&record, Ok(Some(_)));
            if input.blocking_send(record).is_err() || terminal {
                break;
            }
        }
    });
    let loop_result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => Ok(()),
        result = async {
        loop {
            let record = tokio::select! {
                _ = cancellation.cancelled() => break,
                record = incoming.recv() => match record { Some(record) => record?, None => break },
                _ = tokio::signal::ctrl_c() => break,
            };
            let Some(record) = record else { break; };
            let quit = is_shutdown(&record);
            let response = tokio::select! {
                _ = cancellation.cancelled() => break,
                response = rpc::handle_bytes(&ctx, Profile::Full, &record) => response,
            };
            if let Some(response) = response {
                output.send(response).await?;
            }
            // After the reply, exit and drain sessions; HostLifetime owns shared shutdown.
            if quit { break; }
        }
        Ok::<_, anyhow::Error>(())
        } => result,
    };
    drop(incoming);
    wake_driver.shutdown().await;
    drop(wake_registration);
    event_forwarder.abort();
    let _ = event_forwarder.await;
    drop(output);
    let until = tokio::time::Instant::now() + Duration::from_secs(2);
    while !writer.is_finished() && tokio::time::Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if writer.is_finished() {
        writer
            .join()
            .map_err(|_| anyhow::anyhow!("stdout writer panicked"))??;
    }
    loop_result
}

/// A valid request naming `shutdown` ends the loop once its reply is written.
/// The envelope checks match `rpc::handle`, so an invalid record never quits.
fn is_shutdown(record: &[u8]) -> bool {
    let Ok(request) = serde_json::from_slice::<Value>(record) else {
        return false;
    };
    let id_valid = request
        .get("id")
        .is_none_or(|id| id.is_string() || id.is_number() || id.is_null());
    request["jsonrpc"] == "2.0" && request["method"].as_str() == Some("shutdown") && id_valid
}

fn read_record(reader: &mut impl std::io::BufRead) -> Result<Option<Vec<u8>>> {
    let mut record = Vec::new();
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok((!record.is_empty()).then_some(record));
        }
        let end = buffer
            .iter()
            .position(|b| *b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(buffer.len());
        ensure!(
            record.len() + end <= 1024 * 1024,
            "protocol record exceeds 1 MiB"
        );
        let complete = buffer[end - 1] == b'\n';
        record.extend_from_slice(&buffer[..end]);
        reader.consume(end);
        if complete {
            return Ok(Some(record));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_shutdown;

    #[test]
    fn shutdown_quits_only_for_a_valid_request_envelope() {
        assert!(is_shutdown(
            br#"{"jsonrpc":"2.0","id":1,"method":"shutdown"}"#
        ));
        assert!(is_shutdown(br#"{"jsonrpc":"2.0","method":"shutdown"}"#));
        assert!(!is_shutdown(br#"{"jsonrpc":"wrong","method":"shutdown"}"#));
        assert!(!is_shutdown(
            br#"{"jsonrpc":"2.0","id":{},"method":"shutdown"}"#
        ));
        assert!(!is_shutdown(br#"{"jsonrpc":"2.0","id":1,"method":7}"#));
        assert!(!is_shutdown(br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#));
        assert!(!is_shutdown(b"not json"));
    }
}
