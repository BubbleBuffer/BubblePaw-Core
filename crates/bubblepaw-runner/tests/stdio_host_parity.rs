//! Exercise the actual JSON-RPC dispatcher with isolated, offline hosts.
#[path = "support/fixture_binary.rs"]
mod fixture_binary;

use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const SECRET: &str = "synthetic-private-stdio-key";
const CONFIG: &str = "\
[model]
provider='demo'
main='demo/first'
[providers.demo]
kind='scripted'
key_env='BP_STDIO_TEST_API_KEY'
models=[{id='first'},{id='second'}]
[context]
auto_summarize=false
[agents]
auto_wake=false
";

struct Host {
    child: tokio::process::Child,
    input: tokio::process::ChildStdin,
    output: BufReader<tokio::process::ChildStdout>,
    next: u64,
}

impl Host {
    fn start(root: &Path, extra: &[&str]) -> Self {
        let mut command = tokio::process::Command::new(fixture_binary::resolve(
            env!("CARGO_BIN_EXE_bubblepaw"),
            "bubblepaw",
        ));
        command.env_clear().env("BP_STDIO_TEST_API_KEY", SECRET);
        #[cfg(windows)]
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", system_root);
        }
        let mut child = command
            .arg("--config")
            .arg(root.join("config.toml"))
            .arg("--workspace")
            .arg(root)
            .arg("--data-dir")
            .arg(root.join("data"))
            .args(extra)
            .arg("stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        Self {
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
            next: 1,
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        let mut bytes =
            serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
                .unwrap();
        bytes.push(b'\n');
        self.input.write_all(&bytes).await.unwrap();
        self.input.flush().await.unwrap();
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let mut line = String::new();
                assert!(self.output.read_line(&mut line).await.unwrap() > 0);
                let response: Value = serde_json::from_str(&line).unwrap();
                if response["id"] == id {
                    return response;
                }
            }
        })
        .await
        .unwrap()
    }

    async fn ok(&mut self, method: &str, params: Value) -> Value {
        let response = self.call(method, params).await;
        assert!(response.get("error").is_none(), "{method}: {response}");
        response["result"].clone()
    }

    async fn open(&mut self, params: Value) -> String {
        self.ok("session.open", params).await["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    async fn stop(mut self) {
        self.ok("shutdown", json!({})).await;
        assert!(
            tokio::time::timeout(Duration::from_secs(10), self.child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
}

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("config.toml"), CONFIG).unwrap();
    root
}

#[tokio::test]
async fn initialize_advertises_host_parity_without_changing_protocol() {
    let root = fixture();
    let mut host = Host::start(root.path(), &[]);
    let initialized = host.ok("initialize", json!({})).await;
    assert_eq!(initialized["protocol_version"], "0.1");
    for feature in [
        "providers.list",
        "roles.set_main",
        "session.list.pagination",
        "child.cancel",
    ] {
        assert!(
            initialized["features"]
                .as_array()
                .unwrap()
                .contains(&json!(feature))
        );
    }
    host.stop().await;
}

#[path = "stdio_host_parity/children.rs"]
mod children;
#[path = "stdio_host_parity/providers.rs"]
mod providers;
#[path = "stdio_host_parity/sessions.rs"]
mod sessions;
