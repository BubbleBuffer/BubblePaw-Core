//! Starts the web host on an ephemeral port and reads the address it bound.
//! Nothing is reserved beforehand, so no other process can take the port first.

use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
};

/// Startup line prefix `bubblepaw web` prints on stderr once its listener is bound.
const STARTUP_PREFIX: &str = "BubblePaw workbench: ";

/// Run `command` as `web --port 0 --assets <assets>`; return the child and its base URL.
pub async fn start(mut command: Command, assets: &Path) -> (Child, String) {
    let mut child = command
        .args(["web", "--port", "0"])
        .arg("--assets")
        .arg(assets)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let base = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let mut line = String::new();
            assert!(
                stderr.read_line(&mut line).await.unwrap() > 0,
                "web host exited before printing its address"
            );
            if let Some(base) = line.trim_end().strip_prefix(STARTUP_PREFIX) {
                break base.to_owned();
            }
        }
    })
    .await
    .expect("web host did not print its address");
    // Keep draining stderr so the host never blocks on a full pipe.
    tokio::spawn(async move {
        let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
    });
    (child, base)
}
