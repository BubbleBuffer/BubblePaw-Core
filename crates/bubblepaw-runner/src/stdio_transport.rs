//! Stdio transport: blocking stdin/stdout threads around the shared session
//! loop in `rpc_session`. The host context owns wake-ups and shutdown.
use anyhow::Result;
use bubblepaw_core::CancellationToken;
use bubblepaw_runner::host_api::{HostCtx, Profile, ReadScope};
use bubblepaw_runner::rpc_session;
use serde_json::Value;
use std::time::Duration;
use tokio::sync::mpsc;

pub(crate) async fn run(mut ctx: HostCtx, cancellation: CancellationToken) -> Result<()> {
    ctx.read_scope = ReadScope::Workspace(ctx.workspace.clone());
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
    let (input, incoming) = mpsc::channel(16);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        loop {
            let record = rpc_session::read_record(&mut stdin);
            let terminal = !matches!(&record, Ok(Some(_)));
            if input.blocking_send(record).is_err() || terminal {
                break;
            }
        }
    });
    let loop_result = rpc_session::serve(
        &ctx,
        Profile::Full,
        incoming,
        output,
        &cancellation,
        tokio::signal::ctrl_c,
    )
    .await;
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
