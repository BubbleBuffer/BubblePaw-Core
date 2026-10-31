//! Signed-in MCP servers: one user-only JSON file per server under the data
//! directory's `auth` folder, beside the other BubblePaw-owned logins. Never
//! SQLite, never the config file, never logs. Writes are atomic (temporary file,
//! then rename) and refreshes take a per-server lock file so two processes never
//! spend one refresh token.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    io::{Read, Write},
    path::{Path, PathBuf},
};

const FILE_LIMIT: u64 = 1024 * 1024;

/// Tokens for one server URL. `Debug` never shows a secret.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Tokens {
    /// The configured MCP server URL these tokens were issued for. A server
    /// whose URL changed never sends them.
    pub server_url: String,
    /// The RFC 8707 resource sent with every grant.
    pub resource: String,
    pub issuer: String,
    pub token_endpoint: String,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds; absent when the server gave no lifetime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

impl fmt::Debug for Tokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tokens")
            .field("issuer", &self.issuer)
            .field("expires_at", &self.expires_at)
            .field("refreshable", &self.refresh_token.is_some())
            .finish_non_exhaustive()
    }
}

impl Tokens {
    pub(crate) fn secrets(&self) -> impl Iterator<Item = &str> {
        [Some(&self.access_token), self.refresh_token.as_ref()]
            .into_iter()
            .chain([self.client_secret.as_ref()])
            .flatten()
            .map(String::as_str)
    }
    /// Refresh a minute early so a request never carries a token that expires in flight.
    pub(crate) fn expiring(&self, now: u64) -> bool {
        self.expires_at.is_some_and(|at| at <= now + 60)
    }
}

/// `<auth dir>/mcp-<server>.json`. Names outside `[A-Za-z0-9_-]` use a hash.
pub(crate) fn path(dir: &Path, server: &str) -> PathBuf {
    let plain = (1..=64).contains(&server.len())
        && server
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte));
    let stem = if plain {
        server.to_owned()
    } else {
        let digest = Sha256::digest(server.as_bytes());
        format!(
            "h{}",
            digest[..12]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        )
    };
    dir.join(format!("mcp-{stem}.json"))
}

/// A missing, oversized or malformed file is no sign-in.
pub(crate) fn read(path: &Path) -> Option<Tokens> {
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(FILE_LIMIT + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > FILE_LIMIT {
        return None;
    }
    serde_json::from_slice::<Tokens>(&bytes)
        .ok()
        .filter(|tokens| !tokens.access_token.is_empty())
}

pub(crate) fn write(path: &Path, tokens: &Tokens) -> Result<()> {
    let directory = path
        .parent()
        .context("MCP OAuth token path has no folder")?;
    std::fs::create_dir_all(directory).context("cannot create the MCP OAuth token folder")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
            .context("cannot restrict the MCP OAuth token folder")?;
    }
    let bytes = serde_json::to_vec_pretty(tokens)?;
    let mut temporary = tempfile_in(directory)?;
    temporary
        .1
        .write_all(&bytes)
        .and_then(|()| temporary.1.sync_all())
        .context("cannot write MCP OAuth tokens")?;
    drop(temporary.1);
    std::fs::rename(&temporary.0, path)
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&temporary.0);
        })
        .context("cannot replace MCP OAuth tokens")
}

pub(crate) fn remove(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).context("cannot remove MCP OAuth tokens")
        }
        _ => Ok(()),
    }
}

/// Blocks until this process holds the server's refresh lock.
pub(crate) fn lock(path: &Path) -> Result<std::fs::File> {
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory).context("cannot create the MCP OAuth token folder")?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_extension("lock"))
        .context("cannot open the MCP OAuth lock file")?;
    fs2::FileExt::lock_exclusive(&lock).context("cannot lock MCP OAuth tokens")?;
    Ok(lock)
}

fn tempfile_in(directory: &Path) -> Result<(PathBuf, std::fs::File)> {
    for _ in 0..8 {
        let path = directory.join(format!(".mcp-{}.tmp", &super::random_token()[..16]));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        if let Ok(file) = options.open(&path) {
            return Ok((path, file));
        }
    }
    anyhow::bail!("cannot create a temporary MCP OAuth token file")
}
