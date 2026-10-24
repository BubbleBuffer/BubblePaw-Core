//! Opt-in, body-only HTTP diagnostics. Writes are immediate so a dropped future
//! leaves the bytes already received available for inspection.
use serde_json::Value;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);
const ENV: &str = "BUBBLEPAW_PROVIDER_DUMP_DIR";

#[derive(Clone)]
pub(super) struct HttpDump(Arc<Mutex<Option<File>>>);

impl HttpDump {
    /// Called once per physical send. No request builder or headers enter here.
    pub(super) fn begin(body: &Value, fallback_kind: &str) -> Option<Self> {
        let directory = std::env::var_os(ENV).filter(|path| !path.is_empty())?;
        let kind = match bubblepaw_core::current_request_kind() {
            Some(bubblepaw_core::RequestKind::Main) => "main",
            Some(bubblepaw_core::RequestKind::SummaryFork) => "summary_fork",
            Some(bubblepaw_core::RequestKind::Decider) => "decider",
            None => fallback_kind,
        };
        let result = (|| -> std::io::Result<Self> {
            let directory = std::path::Path::new(&directory);
            fs::create_dir_all(directory)?;
            let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.9fZ");
            loop {
                let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
                let stem = format!("{timestamp}-{kind}-{sequence}");
                let request_path = directory.join(format!("{stem}.request.json"));
                let response_path = directory.join(format!("{stem}.response.txt"));
                let mut request = match create(&request_path) {
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    result => result?,
                };
                let response = match create(&response_path) {
                    Ok(file) => file,
                    Err(error) => {
                        drop(request);
                        let _ = fs::remove_file(&request_path);
                        if error.kind() == std::io::ErrorKind::AlreadyExists {
                            continue;
                        }
                        return Err(error);
                    }
                };
                // reqwest's `.json(body)` serializes this same Value to compact JSON.
                serde_json::to_writer(&mut request, body)?;
                return Ok(Self(Arc::new(Mutex::new(Some(response)))));
            }
        })();
        match result {
            Ok(dump) => Some(dump),
            Err(error) => {
                warn(error.kind());
                None
            }
        }
    }

    pub(super) fn attach(self, response: &mut reqwest::Response) {
        response.extensions_mut().insert(self);
    }

    fn append(&self, bytes: &[u8]) {
        let Ok(mut file) = self.0.lock() else { return };
        if let Some(writer) = file.as_mut()
            && let Err(error) = writer.write_all(bytes)
        {
            warn(error.kind());
            *file = None;
        }
    }
}

fn create(path: &std::path::Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn warn(kind: std::io::ErrorKind) {
    // Error messages and paths can contain private data. Only report the category.
    tracing::warn!(target: "bubblepaw::provider", ?kind, "provider HTTP dump write failed");
}

pub(super) fn enabled(response: &reqwest::Response) -> bool {
    response.extensions().get::<HttpDump>().is_some()
}

pub(super) fn append(response: &reqwest::Response, bytes: &[u8]) {
    if let Some(dump) = response.extensions().get::<HttpDump>() {
        dump.append(bytes);
    }
}
