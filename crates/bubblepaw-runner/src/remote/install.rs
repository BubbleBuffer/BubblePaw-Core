//! `bubblepaw remote tunnel install`: download the pinned `cloudflared`
//! release from GitHub into the data directory and check its SHA-256 before
//! keeping it. Nothing downloads `cloudflared` on its own.
use super::cloudflared::program_name;
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The `cloudflared` release `remote tunnel install` fetches.
pub const CLOUDFLARED_VERSION: &str = "2026.10.1";
/// GitHub's release-by-tag API for `cloudflared`.
pub const RELEASES_API: &str = "https://api.github.com/repos/cloudflare/cloudflared/releases/tags";
/// Largest download accepted.
const DOWNLOAD_LIMIT: u64 = 200 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

/// Where the release comes from: GitHub, or a stand-in in tests.
#[derive(Clone, Debug)]
pub struct InstallSource {
    /// Base of the release-by-tag API; the version is appended.
    pub api: String,
    pub version: String,
}

impl Default for InstallSource {
    fn default() -> Self {
        Self {
            api: RELEASES_API.to_owned(),
            version: CLOUDFLARED_VERSION.to_owned(),
        }
    }
}

#[derive(Deserialize)]
struct Release {
    #[serde(default)]
    body: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    digest: Option<String>,
}

/// The release asset for this platform. macOS ships archives only; use
/// Homebrew there and put `cloudflared` on PATH.
pub fn asset_name() -> Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "cloudflared-linux-amd64",
        ("linux", "aarch64") => "cloudflared-linux-arm64",
        ("linux", "arm") => "cloudflared-linux-arm",
        ("windows", "x86_64") => "cloudflared-windows-amd64.exe",
        ("windows", "x86") => "cloudflared-windows-386.exe",
        (os, arch) => bail!(
            "no cloudflared download for {os}/{arch}; install it with your package manager \
             and put it on PATH, or set remote.cloudflared"
        ),
    })
}

/// The expected SHA-256 of `asset`: GitHub's asset digest and the checksum
/// line in the release notes. When both are there they must agree.
fn expected_sha256(release: &Release, asset: &Asset) -> Result<String> {
    let digest = asset
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .map(str::to_ascii_lowercase);
    let prefix = format!("{}:", asset.name);
    let listed = release.body.lines().find_map(|line| {
        line.trim()
            .strip_prefix(&prefix)
            .map(|hex| hex.trim().to_ascii_lowercase())
    });
    let sha = match (digest, listed) {
        (Some(a), Some(b)) if a != b => {
            bail!(
                "the release lists two different checksums for {}",
                asset.name
            )
        }
        (Some(sha), _) | (None, Some(sha)) => sha,
        (None, None) => bail!("the release has no checksum for {}", asset.name),
    };
    if sha.len() != 64 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("the release's checksum for {} is malformed", asset.name);
    }
    Ok(sha)
}

/// Download, verify and install `cloudflared` into `bin_dir`. On any failure
/// nothing is left behind: no partial file, and no directory this call made.
pub async fn install(bin_dir: &Path, source: &InstallSource) -> Result<PathBuf> {
    let created: Vec<PathBuf> = bin_dir
        .ancestors()
        .take_while(|dir| !dir.as_os_str().is_empty() && !dir.exists())
        .map(Path::to_owned)
        .collect();
    let result = download(bin_dir, source).await;
    if result.is_err() {
        // Deepest first; only empty directories go.
        for dir in &created {
            let _ = std::fs::remove_dir(dir);
        }
    }
    result
}

async fn download(bin_dir: &Path, source: &InstallSource) -> Result<PathBuf> {
    let name = asset_name()?;
    let http = reqwest::Client::builder()
        .user_agent(concat!("bubblepaw/", env!("CARGO_PKG_VERSION")))
        .timeout(REQUEST_TIMEOUT)
        .build()?;
    let url = format!("{}/{}", source.api.trim_end_matches('/'), source.version);
    let release: Release = http
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .with_context(|| format!("could not read cloudflared release {}", source.version))?
        .json()
        .await
        .context("unreadable release description")?;
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == name)
        .with_context(|| format!("release {} has no {name}", source.version))?;
    let expected = expected_sha256(&release, asset)?;

    std::fs::create_dir_all(bin_dir)
        .with_context(|| format!("could not create {}", bin_dir.display()))?;
    // Removed on drop unless it is kept below.
    let mut file = tempfile::NamedTempFile::new_in(bin_dir)?;
    let response = http
        .get(&asset.browser_download_url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .with_context(|| format!("could not download {name}"))?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut body = response.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.with_context(|| format!("download of {name} failed"))?;
        size += chunk.len() as u64;
        if size > DOWNLOAD_LIMIT {
            bail!("{name} is larger than {DOWNLOAD_LIMIT} bytes; refused");
        }
        hasher.update(&chunk);
        file.write_all(&chunk)?;
    }
    let actual: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if actual != expected {
        bail!("{name} does not match its published SHA-256; nothing was installed");
    }
    file.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o755))?;
    }
    let target = bin_dir.join(program_name());
    file.persist(&target)
        .with_context(|| format!("could not write {}", target.display()))?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::{Asset, Release, expected_sha256};

    fn release(body: &str, digest: Option<&str>) -> (Release, Asset) {
        let asset = Asset {
            name: "cloudflared-linux-amd64".to_owned(),
            browser_download_url: String::new(),
            digest: digest.map(str::to_owned),
        };
        let release = Release {
            body: body.to_owned(),
            assets: Vec::new(),
        };
        (release, asset)
    }

    #[test]
    fn checksums_come_from_the_digest_or_the_release_notes() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let notes = format!("### SHA256 Checksums:\ncloudflared-linux-amd64: {a}\n");
        let (r, asset) = release(&notes, None);
        assert_eq!(expected_sha256(&r, &asset).unwrap(), a);
        let (r, asset) = release("", Some(&format!("sha256:{b}")));
        assert_eq!(expected_sha256(&r, &asset).unwrap(), b);
        let (r, asset) = release(&notes, Some(&format!("sha256:{b}")));
        assert!(expected_sha256(&r, &asset).is_err());
        let (r, asset) = release("", None);
        assert!(expected_sha256(&r, &asset).is_err());
        let (r, asset) = release("cloudflared-linux-amd64: xyz", None);
        assert!(expected_sha256(&r, &asset).is_err());
    }
}
