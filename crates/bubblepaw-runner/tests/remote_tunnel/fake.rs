//! The fake `cloudflared` fixture: a copy of `bubblepaw-fake-cloudflared` in
//! its own directory with the sidecar that scripts it.
use std::path::{Path, PathBuf};

pub struct FakeCloudflared {
    pub program: PathBuf,
    pub log: PathBuf,
}

/// Install the fake in `dir`. Serving runs print `lines` to stderr (`{n}` is
/// the run number); the first one exits after `exit_first_after_ms`.
pub fn install(dir: &Path, lines: &[&str], exit_first_after_ms: Option<u64>) -> FakeCloudflared {
    std::fs::create_dir_all(dir).unwrap();
    let name = if cfg!(windows) {
        "cloudflared.exe"
    } else {
        "cloudflared"
    };
    let program = dir.join(name);
    std::fs::copy(env!("CARGO_BIN_EXE_bubblepaw-fake-cloudflared"), &program).unwrap();
    let log = dir.join("runs.log");
    let sidecar = serde_json::json!({
        "log": log,
        "lines": lines,
        "exit_first_after_ms": exit_first_after_ms,
        "version": bubblepaw_runner::remote::CLOUDFLARED_VERSION,
    });
    std::fs::write(dir.join("fake-cloudflared.json"), sidecar.to_string()).unwrap();
    FakeCloudflared { program, log }
}

/// Change the executable fingerprint while retaining a runnable fixture.
pub fn invalidate(program: &Path) {
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(program)
        .unwrap()
        .write_all(&[0])
        .unwrap();
}

impl FakeCloudflared {
    /// The arguments of every run so far.
    pub fn runs(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}
