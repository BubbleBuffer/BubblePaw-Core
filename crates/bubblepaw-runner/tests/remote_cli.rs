//! Pairing and revoking while `bubblepaw remote serve` runs: the real binary
//! serves a quick tunnel (a fake `cloudflared` in front of its endpoint), a
//! second `bubblepaw remote pair` process pairs a device through the running
//! server, the device connects over the endpoint without a restart, and
//! `remote devices revoke` from a third process shuts it out.
#[path = "remote_tunnel/fake.rs"]
mod fake;

use bubblepaw_runner::remote::{
    self, PairingPayload, RemoteAddress, RemoteConnect, RemoteError, RemotePair,
};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(60);
const QUICK_URL: &str = "https://cli-test.trycloudflare.com";

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn cli(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bubblepaw"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("BUBBLEPAW_") {
            command.env_remove(key);
        }
    }
    command
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env("HOME", root.join("home"))
        .arg("--config")
        .arg(root.join("config.toml"))
        .arg("--workspace")
        .arg(root)
        .arg("--data-dir")
        .arg(root.join("data"));
    command
}

/// Lines of a child's output, read on a thread.
fn lines(stream: impl std::io::Read + Send + 'static) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

fn next_matching(lines: &mpsc::Receiver<String>, what: &str, seen: &mut Vec<String>) -> String {
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match lines.recv_timeout(left) {
            Ok(line) if line.contains(what) => return line,
            Ok(line) => seen.push(line),
            Err(_) => panic!("no line with {what:?}; output so far: {seen:#?}"),
        }
    }
}

struct Killed(Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn devices_pair_and_revoke_while_serve_runs() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let fake = fake::install(
        &root.join("bin"),
        &[&format!("INF |  {QUICK_URL}  |")],
        None,
    );
    let port = free_port();
    let config = format!(
        "[model]\nprovider='demo'\nmain='demo/first'\n[providers.demo]\nkind='scripted'\n\
         models=[{{id='first'}}]\n[context]\nauto_summarize=false\n[agents]\nauto_wake=false\n\
         [remote]\nenabled=true\nmode='quick'\nport={port}\ncloudflared={:?}\n",
        fake.program.to_string_lossy()
    );
    std::fs::write(root.join("config.toml"), config).unwrap();

    let mut serve = cli(root)
        .args(["remote", "serve"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let serve_log = lines(serve.stderr.take().unwrap());
    let serve = Killed(serve);
    let mut seen = Vec::new();
    let line = next_matching(&serve_log, "Devices reach this host at", &mut seen);
    assert!(line.ends_with("wss://cli-test.trycloudflare.com"), "{line}");

    // Pair through the running server from a second process.
    let mut pairing = cli(root)
        .args(["--json", "remote", "pair", "--profile", "full"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pair_out = lines(pairing.stdout.take().unwrap());
    let pair_err = lines(pairing.stderr.take().unwrap());
    let mut answer = pairing.stdin.take().unwrap();
    let mut pair_seen = Vec::new();
    let offer: Value =
        serde_json::from_str(&next_matching(&pair_out, "\"offer\"", &mut pair_seen)).unwrap();
    let payload = PairingPayload::parse(offer["payload"].as_str().unwrap()).unwrap();
    assert_eq!(
        payload.direct.as_deref(),
        Some("wss://cli-test.trycloudflare.com")
    );
    assert_eq!(payload.code, offer["code"].as_str().unwrap());

    // The device dials the endpoint directly, as the Cloudflare edge would.
    let local = RemoteAddress::Direct(format!("ws://127.0.0.1:{port}"));
    let device = root.join("device");
    let pending = remote::pair(RemotePair {
        address: local.clone(),
        host_id: payload.host.clone(),
        code: payload.code.clone(),
        key_file: device.join("device.bpkey"),
        hosts: device.join("hosts.json"),
        device_name: "CLI phone".into(),
        device_kind: bubblepaw_runner::remote::DeviceKind::Other,
        limits: Default::default(),
    })
    .await
    .unwrap();
    let request: Value =
        serde_json::from_str(&next_matching(&pair_out, "\"request\"", &mut pair_seen)).unwrap();
    assert_eq!(request["device_name"], "CLI phone");
    assert_eq!(request["comparison"], pending.comparison());
    writeln!(answer, "y").unwrap();
    let host = pending.finish().await.unwrap();
    assert_eq!(host.id, payload.host);
    let paired: Value =
        serde_json::from_str(&next_matching(&pair_out, "\"paired\"", &mut pair_seen)).unwrap();
    assert_eq!(paired["profile"], "full");
    let device_id = paired["id"].as_str().unwrap().to_owned();
    let status = pairing.wait().unwrap();
    let errors: Vec<String> = pair_err.try_iter().collect();
    assert!(status.success(), "{errors:#?}");

    // The running server admits the new device without a restart.
    let connect = || RemoteConnect {
        address: local.clone(),
        host_id: payload.host.clone(),
        key_file: device.join("device.bpkey"),
        hosts: device.join("hosts.json"),
        limits: Default::default(),
    };
    let client = remote::connect(connect()).await.unwrap();
    let session = client
        .call("session.open", json!({"title": "over the tunnel"}))
        .await
        .unwrap();
    let id = session["id"].as_str().unwrap().to_owned();
    client
        .call("run.start", json!({"session_id": id, "text": "Hello"}))
        .await
        .unwrap();
    let run = client
        .call("run.wait", json!({"session_id": id}))
        .await
        .unwrap();
    assert_eq!(run["status"], "completed", "{run}");

    // Revoke from a third process: the server closes the tunnel and refuses
    // the device from then on.
    let revoked = cli(root)
        .args(["remote", "devices", "revoke", &device_id])
        .output()
        .unwrap();
    assert!(
        revoked.status.success(),
        "{}",
        String::from_utf8_lossy(&revoked.stderr)
    );
    let refused = remote::connect(connect()).await.err();
    assert!(matches!(refused, Some(RemoteError::Refused)), "{refused:?}");
    client.close().await;
    // cloudflared ran once, in front of the configured port.
    let runs = fake.runs();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].last().unwrap(), &format!("http://127.0.0.1:{port}"));
    drop(serve);
}
