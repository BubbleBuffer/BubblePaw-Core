use bubblepaw_remote::relay::{Limits, Relay, RelayConfig};
use std::net::SocketAddr;

#[tokio::main]
async fn main() {
    if let Err(message) = run().await {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), &'static str> {
    let mut args = std::env::args().skip(1);
    let address: SocketAddr = args
        .next()
        .unwrap_or_else(|| "127.0.0.1:8787".into())
        .parse()
        .map_err(|_| "invalid listen address")?;
    if args.next().is_some() {
        return Err("usage: bubblepaw-relay [IP:PORT]");
    }
    let relay = Relay::bind(
        address,
        RelayConfig {
            limits: Limits::default(),
            ..Default::default()
        },
    )
    .await
    .map_err(|_| "relay listener failed")?;
    eprintln!("relay listening on {}", relay.local_addr());
    shutdown_signal().await?;
    relay.shutdown().await;
    Ok(())
}

async fn shutdown_signal() -> Result<(), &'static str> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .map_err(|_| "signal handler failed")?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.map_err(|_| "signal handler failed"),
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c()
        .await
        .map_err(|_| "signal handler failed")
}
