//! What an app's Cloudflare domain page needs: whether the host is signed in
//! (a `cloudflared tunnel login` certificate or an API token), starting that
//! login without opening a browser, the zones and account of the login, and the
//! installed `cloudflared`'s version with an update. Core opens no browser and
//! never prints or returns a token, certificate or login URL anywhere but to the
//! caller that asked.
use super::RemotePaths;
use super::cloudflare_api::{ApiClient, ZoneInfo};
use super::cloudflared::program_name;
use super::cloudflared_validation::numbers;
use super::domain::{TOKEN_ENV, TOKEN_FILE};
use super::install::{self, CLOUDFLARED_VERSION, InstallSource};
use super::keys::write_private;
use super::reach::{CLOUDFLARE_API, CloudflareApi};
use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use zeroize::Zeroizing;

/// How long `cloudflared tunnel login` may take to print its URL.
const URL_WAIT: Duration = Duration::from_secs(30);
/// A certificate larger than this is not one.
const MAX_CERT: u64 = 64 * 1024;
/// The environment variable `cloudflared` reads for its certificate path.
const CERT_ENV: &str = "TUNNEL_ORIGIN_CERT";

/// Where the API token comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenSource {
    /// `BUBBLEPAW_CLOUDFLARE_TOKEN`.
    Env,
    /// The owner-only `<data_dir>/remote/cloudflare/api-token`.
    File,
}

/// Whether the host can set up domain mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct CloudflareLogin {
    /// A `cloudflared tunnel login` certificate exists.
    pub cert: bool,
    /// An API token is configured (it wins over the certificate).
    pub token: Option<TokenSource>,
}

impl CloudflareLogin {
    /// Either credential is there.
    pub fn signed_in(&self) -> bool {
        self.cert || self.token.is_some()
    }
}

/// `cloudflared`'s certificate: `cert` if given, else `TUNNEL_ORIGIN_CERT`,
/// else `~/.cloudflared/cert.pem`.
fn cert_path(cert: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = cert {
        return Some(path.to_owned());
    }
    if let Some(path) = std::env::var_os(CERT_ENV).filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|home| !home.is_empty())?;
    Some(PathBuf::from(home).join(".cloudflared").join("cert.pem"))
}

fn token_source(paths: &RemotePaths) -> Option<TokenSource> {
    if std::env::var(TOKEN_ENV).is_ok_and(|token| !token.trim().is_empty()) {
        return Some(TokenSource::Env);
    }
    let file = std::fs::read_to_string(paths.cloudflare.join(TOKEN_FILE)).ok()?;
    (!file.trim().is_empty()).then_some(TokenSource::File)
}

/// Whether a certificate and/or an API token is configured. `cert` overrides
/// `cloudflared`'s default certificate location (for tests and non-default
/// setups).
pub fn cloudflare_login_state(paths: &RemotePaths, cert: Option<&Path>) -> CloudflareLogin {
    CloudflareLogin {
        cert: cert_path(cert).is_some_and(|path| path.is_file()),
        token: token_source(paths),
    }
}

/// Store an API token (Cloudflare Tunnel edit and DNS edit) in the owner-only
/// token file, as `bubblepaw remote tunnel token` does. The token is a secret.
pub fn store_cloudflare_token(paths: &RemotePaths, token: &str) -> Result<()> {
    let token = Zeroizing::new(token.trim().to_owned());
    if token.is_empty() || token.len() > 4096 || token.chars().any(char::is_whitespace) {
        bail!("expected one API token");
    }
    write_private(&paths.cloudflare.join(TOKEN_FILE), token.as_bytes())
}

/// Remove the stored API token file. The `BUBBLEPAW_CLOUDFLARE_TOKEN`
/// environment variable and `cloudflared`'s certificate are not touched.
/// Returns false when there was no file.
pub fn forget_cloudflare_token(paths: &RemotePaths) -> Result<bool> {
    match std::fs::remove_file(paths.cloudflare.join(TOKEN_FILE)) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("could not remove the stored API token"),
    }
}

/// Forget only the browser sign-in certificate. Tunnel credentials, DNS,
/// named-tunnel state and API tokens remain available.
pub fn forget_cloudflare_login(cert: Option<&Path>) -> Result<bool> {
    let Some(path) = cert_path(cert) else {
        return Ok(false);
    };
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("could not remove the Cloudflare login certificate"),
    }
}

/// A zone of the signed-in account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CloudflareZone {
    pub id: String,
    /// The domain, such as `example.com`.
    pub name: String,
}

/// The signed-in account and the zones a hostname can live in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CloudflareAccount {
    /// The account's name, when the API said.
    pub name: Option<String>,
    pub zones: Vec<CloudflareZone>,
}

/// The pieces of `cloudflared`'s certificate the zone lookup needs.
struct CertLogin {
    zone_id: String,
    token: Zeroizing<String>,
}

/// Read the `ARGO TUNNEL TOKEN` block of a `cloudflared` certificate: base64 of
/// JSON with `zoneID` and `apiToken`. Only those fields are used, in memory,
/// for read-only calls to the Cloudflare API.
fn read_cert(path: &Path) -> Result<CertLogin> {
    use std::io::Read;
    let file =
        std::fs::File::open(path).with_context(|| format!("could not open {}", path.display()))?;
    let mut text = Zeroizing::new(String::new());
    file.take(MAX_CERT + 1)
        .read_to_string(&mut text)
        .with_context(|| format!("could not read {}", path.display()))?;
    if text.len() as u64 > MAX_CERT {
        bail!("{} is not a cloudflared certificate", path.display());
    }
    let mut body = Zeroizing::new(String::new());
    let mut inside = false;
    for line in text.lines() {
        match line.trim() {
            "-----BEGIN ARGO TUNNEL TOKEN-----" => inside = true,
            "-----END ARGO TUNNEL TOKEN-----" => break,
            line if inside => body.push_str(line),
            _ => {}
        }
    }
    let bytes = Zeroizing::new(
        base64::engine::general_purpose::STANDARD
            .decode(body.as_bytes())
            .context("the cloudflared certificate has no readable login")?,
    );
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .context("the cloudflared certificate has no readable login")?;
    let zone_id = value["zoneID"].as_str().unwrap_or_default().to_owned();
    let token = Zeroizing::new(value["apiToken"].as_str().unwrap_or_default().to_owned());
    if zone_id.is_empty() || token.is_empty() {
        bail!("the cloudflared certificate has no zone login");
    }
    Ok(CertLogin { zone_id, token })
}

fn account(zones: Vec<ZoneInfo>, fallback: Option<String>) -> CloudflareAccount {
    let name = zones
        .iter()
        .find_map(|zone| zone.account_name.clone())
        .or(fallback);
    CloudflareAccount {
        name,
        zones: zones
            .into_iter()
            .map(|zone| CloudflareZone {
                id: zone.id,
                name: zone.name,
            })
            .collect(),
    }
}

/// The account name and zones of the host's Cloudflare login, or `None` when
/// nobody is signed in. With an API token, it lists the zones the token can
/// see; with only a `cloudflared tunnel login` certificate, it looks up the
/// zone that login was made for (a certificate names one). `base_url` is the
/// Cloudflare API (`None` for the real one; tests pass a local fake).
pub async fn cloudflare_account(
    paths: &RemotePaths,
    cert: Option<&Path>,
    base_url: Option<&str>,
) -> Result<Option<CloudflareAccount>> {
    let base = base_url.unwrap_or(CLOUDFLARE_API);
    if token_source(paths).is_some() {
        let token =
            super::domain::api_token(&paths.cloudflare)?.context("the API token vanished")?;
        let mut api = CloudflareApi::new(token);
        api.base_url = base.to_owned();
        let client = ApiClient::new(&api)?;
        let zones = client.zones().await?;
        let fallback = if zones.iter().any(|zone| zone.account_name.is_some()) {
            None
        } else {
            client.first_account_name().await
        };
        return Ok(Some(account(zones, fallback)));
    }
    let Some(cert) = cert_path(cert).filter(|path| path.is_file()) else {
        return Ok(None);
    };
    let login = read_cert(&cert)?;
    let mut api = CloudflareApi::new(login.token.to_string());
    api.base_url = base.to_owned();
    let zone = ApiClient::new(&api)?.zone(&login.zone_id).await?;
    Ok(Some(account(vec![zone], None)))
}

/// A `cloudflared tunnel login` in progress. Open [`Self::url`] in a browser
/// (Core opens none), then [`Self::wait`] for the login to finish. Dropping it
/// stops `cloudflared`. The URL is a one-time login link: show it only to the
/// person logging in and never log it.
pub struct TunnelLogin {
    url: String,
    child: Child,
    cert: Option<PathBuf>,
    _readers: Vec<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for TunnelLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelLogin").finish_non_exhaustive()
    }
}

impl TunnelLogin {
    /// The page to open to log in to Cloudflare.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Wait for `cloudflared` to finish (the browser login completes and the
    /// certificate is written), up to `timeout`.
    pub async fn wait(mut self, timeout: Duration) -> Result<()> {
        let status = tokio::time::timeout(timeout, self.child.wait())
            .await
            .map_err(|_| anyhow::anyhow!("the Cloudflare login did not finish in time"))?
            .context("could not wait for cloudflared")?;
        if !status.success() {
            bail!("cloudflared tunnel login failed ({status})");
        }
        if cert_path(self.cert.as_deref()).is_some_and(|path| !path.is_file()) {
            bail!("cloudflared finished without writing its certificate");
        }
        Ok(())
    }

    /// Stop the login.
    pub async fn cancel(mut self) {
        let _ = self.child.kill().await;
    }
}

fn pump(
    stream: impl AsyncRead + Unpin + Send + 'static,
    lines: mpsc::UnboundedSender<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stream).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            // Keep reading after the URL so the pipe never fills.
            let _ = lines.send(line);
        }
    })
}

/// Start `cloudflared tunnel login` and return the login URL it prints. Core
/// opens no browser: the app shows or opens the URL. `cert` makes
/// `cloudflared` write its certificate there (`TUNNEL_ORIGIN_CERT`); `None`
/// keeps its default location.
pub async fn tunnel_login(cloudflared: &Path, cert: Option<&Path>) -> Result<TunnelLogin> {
    let mut command = Command::new(cloudflared);
    command
        .args(["tunnel", "login"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(cert) = cert {
        command.env(CERT_ENV, cert);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("could not run {}", cloudflared.display()))?;
    let (send, mut lines) = mpsc::unbounded_channel();
    let mut readers = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        readers.push(pump(stdout, send.clone()));
    }
    if let Some(stderr) = child.stderr.take() {
        readers.push(pump(stderr, send.clone()));
    }
    drop(send);
    let url = tokio::time::timeout(URL_WAIT, async {
        while let Some(line) = lines.recv().await {
            if let Some(start) = line.find("https://") {
                let url: String = line[start..]
                    .chars()
                    .take_while(|c| !c.is_whitespace())
                    .collect();
                return Some(url);
            }
        }
        None
    })
    .await;
    match url {
        Ok(Some(url)) => Ok(TunnelLogin {
            url,
            child,
            cert: cert.map(Path::to_owned),
            _readers: readers,
        }),
        Ok(None) => bail!("cloudflared tunnel login ended without printing a login URL"),
        Err(_) => bail!("cloudflared tunnel login printed no login URL in time"),
    }
}

/// The `cloudflared` an app would run and whether the pinned release is newer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CloudflaredInfo {
    pub path: PathBuf,
    /// The installed version, such as `2026.9.0`; `None` if it did not say.
    pub version: Option<String>,
    /// The release `install_cloudflared` fetches ([`CLOUDFLARED_VERSION`]).
    pub pinned: String,
    /// `pinned` is newer than `version`.
    pub update_available: bool,
    /// The copy lives in this data directory's `bin/`, installed by BubblePaw
    /// (only that copy can be updated through [`update_cloudflared`]).
    pub managed: bool,
    /// Explicitly selected by the host user, rather than found by default.
    pub chosen: bool,
}

/// Validate locally, reusing the result while the executable fingerprint is unchanged.
pub async fn cloudflared_info(program: &Path, paths: &RemotePaths) -> Result<CloudflaredInfo> {
    let version = super::cloudflared_validation::version(program).await?;
    Ok(info(program, paths, version))
}

/// Read a matching validation result without starting any process.
pub fn cached_cloudflared_info(
    program: &Path,
    paths: &RemotePaths,
) -> Result<Option<CloudflaredInfo>> {
    Ok(super::cloudflared_validation::cached_version(program)?
        .map(|version| info(program, paths, version)))
}

fn info(program: &Path, paths: &RemotePaths, version: String) -> CloudflaredInfo {
    let version = Some(version);
    let update_available = version
        .as_deref()
        .is_some_and(|version| numbers(version) < numbers(CLOUDFLARED_VERSION));
    CloudflaredInfo {
        path: program.to_owned(),
        version,
        pinned: CLOUDFLARED_VERSION.to_owned(),
        update_available,
        managed: program
            .canonicalize()
            .ok()
            .zip(
                paths
                    .cloudflare_bin()
                    .join(program_name())
                    .canonicalize()
                    .ok(),
            )
            .is_some_and(|(program, managed)| program == managed),
        chosen: false,
    }
}

/// Install (or update to) the pinned `cloudflared` in this data directory's
/// `bin/`, verified against its published SHA-256 like `install_cloudflared`;
/// a mismatch installs nothing. Stop `remote serve` first on Windows, which
/// will not replace a running program. Returns the installed copy's info.
pub async fn update_cloudflared(
    paths: &RemotePaths,
    source: &InstallSource,
) -> Result<CloudflaredInfo> {
    let installed = install::install(&paths.cloudflare_bin(), source).await?;
    debug_assert!(installed.ends_with(program_name()));
    cloudflared_info(&installed, paths).await
}
