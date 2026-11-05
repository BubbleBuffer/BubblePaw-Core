//! One bounded executable check shared by selection, local inspection and serving.
//! Remote inspection can read this cache but never populate it by running a child.
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Mutex, OnceLock},
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};

const VERSION_WAIT: Duration = Duration::from_secs(3);
const CACHE_LIMIT: usize = 128;
static CHECK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static CACHE: OnceLock<Mutex<HashMap<PathBuf, Entry>>> = OnceLock::new();

#[derive(Clone, PartialEq, Eq)]
struct Fingerprint {
    digest: [u8; 32],
    permissions: u32,
}
struct Entry {
    fingerprint: Fingerprint,
    version: Result<String, String>,
}

fn cache() -> &'static Mutex<HashMap<PathBuf, Entry>> {
    CACHE.get_or_init(Mutex::default)
}

fn fingerprint(program: &Path) -> Result<(PathBuf, Fingerprint)> {
    let path = program
        .canonicalize()
        .context("cloudflared program does not exist")?;
    let mut file = std::fs::File::open(&path).context("could not open cloudflared program")?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        bail!("cloudflared program must be a file");
    }
    #[cfg(unix)]
    let permissions = {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode();
        if mode & 0o111 == 0 {
            bail!("cloudflared program is not executable");
        }
        mode
    };
    #[cfg(not(unix))]
    let permissions = u32::from(metadata.permissions().readonly());
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 32768];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok((
        path,
        Fingerprint {
            digest: hash.finalize().into(),
            permissions,
        },
    ))
}

fn lookup(path: &Path, fingerprint: &Fingerprint) -> Result<Option<String>> {
    cache()
        .lock()
        .unwrap()
        .get(path)
        .filter(|entry| &entry.fingerprint == fingerprint)
        .map(|entry| entry.version.clone().map_err(anyhow::Error::msg))
        .transpose()
}

pub(super) fn cached_version(program: &Path) -> Result<Option<String>> {
    // A remote read reports "not checked" for an inaccessible, missing or
    // non-executable file. Only a matching cached check may report a failure.
    let Ok((path, fingerprint)) = fingerprint(program) else {
        return Ok(None);
    };
    lookup(&path, &fingerprint)
}

pub(super) async fn version(program: &Path) -> Result<String> {
    let _checking = CHECK.lock().await;
    let (path, before) = fingerprint(program)?;
    if let Some(version) = lookup(&path, &before)? {
        return Ok(version);
    }
    let result = run(&path).await;
    if fingerprint(program)? != (path.clone(), before.clone()) {
        bail!("cloudflared program changed while its version was checked");
    }
    let saved = result
        .as_ref()
        .cloned()
        .map_err(|error| format!("{error:#}"));
    let mut cache = cache().lock().unwrap();
    if cache.len() >= CACHE_LIMIT && !cache.contains_key(&path) {
        cache.clear();
    }
    cache.insert(
        path,
        Entry {
            fingerprint: before,
            version: saved,
        },
    );
    result
}

async fn run(program: &Path) -> Result<String> {
    let mut command = Command::new(program);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    let mut child = command
        .spawn()
        .with_context(|| format!("could not run {}", program.display()))?;
    let mut stdout = child
        .stdout
        .take()
        .context("cloudflared stdout missing")?
        .take(8192);
    let mut stderr = child
        .stderr
        .take()
        .context("cloudflared stderr missing")?
        .take(8192);
    let mut out = Vec::new();
    let mut err = Vec::new();
    let (status, _, _) = tokio::time::timeout(VERSION_WAIT, async {
        tokio::try_join!(
            child.wait(),
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err)
        )
    })
    .await
    .map_err(|_| anyhow::anyhow!("cloudflared --version did not answer in time"))??;
    if !status.success() {
        bail!("cloudflared --version failed ({status})");
    }
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&err)
    );
    parse_version(&text).context("the selected program did not identify itself as cloudflared")
}

pub(super) fn numbers(version: &str) -> Vec<u64> {
    version
        .split('.')
        .map_while(|part| part.parse().ok())
        .collect()
}

fn parse_version(output: &str) -> Option<String> {
    let rest = output
        .lines()
        .find_map(|line| line.trim().strip_prefix("cloudflared version "))?;
    let version = rest.split_whitespace().next()?;
    let valid = version.chars().all(|c| c.is_ascii_digit() || c == '.')
        && version.chars().next().is_some_and(|c| c.is_ascii_digit());
    valid.then(|| version.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn versions_parse_and_compare_numerically() {
        assert_eq!(
            parse_version("cloudflared version 2026.9.0 (built 2026-09-01T00:00:00Z)"),
            Some("2026.9.0".to_owned())
        );
        assert_eq!(parse_version("another program version 2026.9.0"), None);
        assert!(numbers("2026.9.0") < numbers("2026.10.1"));
    }
}
