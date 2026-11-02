//! Test stand-in for `cloudflared`. Tests copy it into a temporary directory
//! next to a `fake-cloudflared.json` that says what to do:
//!
//! - `log`: a file that gets one JSON line (the arguments) per run;
//! - `lines`: lines printed to stderr when serving a tunnel, with `{n}`
//!   replaced by the run number (1 for the first run);
//! - `exit_first_after_ms`: the first serving run exits after this long.
//!
//! `tunnel create --credentials-file F NAME` writes a credentials file;
//! `tunnel route dns` succeeds. `tunnel login` prints the sidecar's `login_url`
//! after the usual preamble, writes `cert.contents` to `cert.path` and exits;
//! `--version` prints the sidecar's `version`. Serving runs stay up until
//! stdin closes, as the managed process does when its host stops.
use std::io::{Read, Write};
use std::path::PathBuf;

const TUNNEL_ID: &str = "6ff42ae2-765d-4adf-8112-31c55c1551ef";

fn main() {
    let exe = std::env::current_exe().expect("own path");
    let sidecar = exe.with_file_name("fake-cloudflared.json");
    let config: serde_json::Value = std::fs::read(&sidecar)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let run = match config["log"].as_str().map(PathBuf::from) {
        Some(log) => {
            let previous = std::fs::read_to_string(&log)
                .map(|text| text.lines().count())
                .unwrap_or(0);
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log)
                .expect("open the log");
            writeln!(file, "{}", serde_json::json!(args)).expect("write the log");
            previous + 1
        }
        None => 1,
    };
    let words: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|arg| *arg != "--no-autoupdate")
        .collect();
    match words.as_slice() {
        ["tunnel", "create", "--credentials-file", path, _name] => {
            let credentials = serde_json::json!({
                "AccountTag": "fake-account",
                "TunnelSecret": "not-a-real-secret",
                "TunnelID": TUNNEL_ID,
            });
            std::fs::write(path, credentials.to_string()).expect("write credentials");
            return;
        }
        ["tunnel", "route", "dns", ..] => return,
        ["tunnel", "login"] => {
            if let Some(url) = config["login_url"].as_str() {
                eprintln!(
                    "Please open the following URL and log in with your Cloudflare account:\n"
                );
                eprintln!("{url}\n");
                eprintln!("Leave cloudflared running to download the cert automatically.");
            }
            if let (Some(path), Some(contents)) = (
                config["cert"]["path"].as_str(),
                config["cert"]["contents"].as_str(),
            ) {
                std::fs::write(path, contents).expect("write the certificate");
            }
            return;
        }
        ["--version"] => {
            if let Some(version) = config["version"].as_str() {
                println!("cloudflared version {version} (built 2026-01-01T00:00:00Z)");
            }
            return;
        }
        _ => {}
    }
    let mut stderr = std::io::stderr();
    for line in config["lines"].as_array().into_iter().flatten() {
        let line = line
            .as_str()
            .unwrap_or_default()
            .replace("{n}", &run.to_string());
        let _ = writeln!(stderr, "{line}");
    }
    let _ = stderr.flush();
    if run == 1
        && let Some(ms) = config["exit_first_after_ms"].as_u64()
    {
        std::thread::sleep(std::time::Duration::from_millis(ms));
        std::process::exit(1);
    }
    // Serve until the host closes stdin or kills this process.
    let mut sink = [0u8; 64];
    let mut stdin = std::io::stdin();
    while matches!(stdin.read(&mut sink), Ok(n) if n > 0) {}
}
