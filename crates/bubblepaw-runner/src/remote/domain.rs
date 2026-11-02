//! Domain mode setup: one named tunnel per host, created once (through the
//! Cloudflare API with a token, or through `cloudflared` after `remote tunnel
//! login`), its credentials kept in the data directory, and a proxied CNAME
//! from the configured hostname to it.
use super::cloudflare_api::ApiClient;
use super::reach::CloudflareTunnel;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use tokio::process::Command;

/// The credentials file `cloudflared tunnel run --credentials-file` reads.
pub(super) const CREDENTIALS_FILE: &str = "tunnel.json";
/// What setup finished: the hostname, the tunnel and whether DNS points at it.
const STATE_FILE: &str = "state.json";
/// The API token file, when it is not in the environment.
pub const TOKEN_FILE: &str = "api-token";
/// The environment variable that can hold the API token.
pub const TOKEN_ENV: &str = "BUBBLEPAW_CLOUDFLARE_TOKEN";
/// Longest `cloudflared` error output quoted in an error.
const QUOTE_LIMIT: usize = 2000;

/// A named tunnel ready to run.
pub(super) struct NamedTunnel {
    pub credentials: PathBuf,
    pub tunnel_id: String,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct State {
    hostname: String,
    tunnel_id: String,
    dns: bool,
}

/// `cloudflared`'s credentials file. The secret is a secret: never log it.
#[derive(Deserialize, Serialize)]
pub(super) struct Credentials {
    #[serde(rename = "AccountTag")]
    pub account_tag: String,
    #[serde(rename = "TunnelSecret")]
    pub tunnel_secret: String,
    #[serde(rename = "TunnelID")]
    pub tunnel_id: String,
}

/// Check a configured hostname: lowercase DNS labels, at least two.
pub fn check_hostname(hostname: &str) -> Result<()> {
    let labels: Vec<&str> = hostname.split('.').collect();
    let valid = hostname.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        });
    if !valid {
        bail!("remote.hostname must be a lowercase DNS name such as paw.example.com");
    }
    Ok(())
}

/// The API token from the environment or the token file in `dir`.
pub fn api_token(dir: &Path) -> Result<Option<String>> {
    if let Ok(token) = std::env::var(TOKEN_ENV) {
        let token = token.trim().to_owned();
        if !token.is_empty() {
            return Ok(Some(token));
        }
    }
    let path = dir.join(TOKEN_FILE);
    match std::fs::read_to_string(&path) {
        Ok(token) => {
            let token = token.trim().to_owned();
            Ok((!token.is_empty()).then_some(token))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("could not read {}", path.display())),
    }
}

/// Write `contents` to `path`, owner-only on Unix, replacing it atomically.
pub(super) fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path.parent().context("no parent directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    let mut file = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("could not write in {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(contents)?;
    file.as_file().sync_all()?;
    file.persist(path)
        .with_context(|| format!("could not write {}", path.display()))?;
    Ok(())
}

fn read_state(dir: &Path) -> Result<Option<State>> {
    let path = dir.join(STATE_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("{} is not readable", path.display()))
            .map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("could not read {}", path.display())),
    }
}

fn write_state(dir: &Path, state: &State) -> Result<()> {
    write_private(&dir.join(STATE_FILE), &serde_json::to_vec_pretty(state)?)
}

pub(super) fn read_credentials(path: &Path) -> Result<Credentials> {
    let bytes =
        std::fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("{} is not a cloudflared credentials file", path.display()))
}

/// Make sure the named tunnel for `hostname` exists, has credentials here and
/// a DNS route, and return how to run it.
pub(super) async fn prepare(tunnel: &CloudflareTunnel, hostname: &str) -> Result<NamedTunnel> {
    check_hostname(hostname)?;
    let dir = &tunnel.dir;
    let credentials = dir.join(CREDENTIALS_FILE);
    let mut state = match read_state(dir)? {
        Some(state) if state.hostname != hostname => bail!(
            "the tunnel in {} was set up for {}; delete that directory (and the tunnel in \
             Cloudflare) to set up {hostname}",
            dir.display(),
            state.hostname
        ),
        Some(state) if credentials.is_file() => state,
        _ => {
            // Credentials without state: an earlier setup stopped after creating
            // the tunnel; use that tunnel.
            let tunnel_id = match credentials.is_file() {
                true => read_credentials(&credentials)?.tunnel_id,
                false => create(tunnel, hostname, &credentials).await?,
            };
            let state = State {
                hostname: hostname.to_owned(),
                tunnel_id,
                dns: false,
            };
            write_state(dir, &state)?;
            state
        }
    };
    if !state.dns {
        route(tunnel, hostname, &state.tunnel_id).await?;
        state.dns = true;
        write_state(dir, &state)?;
    }
    Ok(NamedTunnel {
        credentials,
        tunnel_id: state.tunnel_id,
    })
}

/// A tunnel name unique per hostname.
fn tunnel_name(hostname: &str) -> String {
    format!("bubblepaw-{}", hostname.replace('.', "-"))
}

async fn create(tunnel: &CloudflareTunnel, hostname: &str, credentials: &Path) -> Result<String> {
    if let Some(api) = &tunnel.api {
        let client = ApiClient::new(api)?;
        let zone = client.zone_for(hostname).await?;
        let secret = random_secret()?;
        let tunnel_id = client
            .create_tunnel(&zone.account_id, &tunnel_name(hostname), &secret)
            .await?;
        let file = Credentials {
            account_tag: zone.account_id,
            tunnel_secret: secret,
            tunnel_id: tunnel_id.clone(),
        };
        write_private(credentials, &serde_json::to_vec(&file)?)?;
        return Ok(tunnel_id);
    }
    std::fs::create_dir_all(&tunnel.dir)
        .with_context(|| format!("could not create {}", tunnel.dir.display()))?;
    let mut command = Command::new(&tunnel.cloudflared);
    command
        .args(["tunnel", "create", "--credentials-file"])
        .arg(credentials)
        .arg(tunnel_name(hostname));
    run(command, "create the tunnel").await?;
    Ok(read_credentials(credentials)?.tunnel_id)
}

async fn route(tunnel: &CloudflareTunnel, hostname: &str, tunnel_id: &str) -> Result<()> {
    if let Some(api) = &tunnel.api {
        let client = ApiClient::new(api)?;
        let zone = client.zone_for(hostname).await?;
        return client.route_dns(&zone.id, hostname, tunnel_id).await;
    }
    let mut command = Command::new(&tunnel.cloudflared);
    command.args(["tunnel", "route", "dns", tunnel_id, hostname]);
    run(command, "route the hostname to the tunnel").await
}

/// Run a setup command of `cloudflared`; quote its error output on failure.
async fn run(mut command: Command, what: &str) -> Result<()> {
    let output = command
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .context("could not run cloudflared")?;
    if output.status.success() {
        return Ok(());
    }
    let text = String::from_utf8_lossy(&output.stderr);
    let mut quoted = text.trim().to_owned();
    if quoted.len() > QUOTE_LIMIT {
        let mut cut = quoted.len() - QUOTE_LIMIT;
        while !quoted.is_char_boundary(cut) {
            cut += 1;
        }
        quoted = format!("...{}", &quoted[cut..]);
    }
    bail!(
        "cloudflared could not {what} ({}): {quoted}\nRun `bubblepaw remote tunnel login` first, \
         or provide a Cloudflare API token ({TOKEN_ENV} or remote/cloudflare/{TOKEN_FILE})",
        output.status
    )
}

/// A fresh 32-byte tunnel secret, base64-encoded as `cloudflared` expects.
fn random_secret() -> Result<String> {
    use base64::Engine;
    let mut bytes = zeroize::Zeroizing::new([0u8; 32]);
    getrandom::fill(bytes.as_mut()).map_err(|error| anyhow::anyhow!("no randomness: {error}"))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::check_hostname;

    #[test]
    fn hostnames_are_checked() {
        for good in ["paw.example.com", "a-b.example.co.uk", "x1.io"] {
            check_hostname(good).unwrap();
        }
        for bad in [
            "example",
            "Paw.example.com",
            "-a.example.com",
            "a..b",
            "a b.com",
            "",
        ] {
            assert!(check_hostname(bad).is_err(), "{bad}");
        }
    }
}
