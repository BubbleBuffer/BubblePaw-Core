//! OpenCode Console device login and Go subscription meters.
//!
//! The Go API key is rejected on console routes; a Console device-login token
//! is accepted. That token belongs to BubblePaw (unlike Claude credentials), so
//! BubblePaw refreshes it and writes the rotated token back atomically. Tokens
//! live only in a user-only file under the data directory: never in SQLite,
//! never in logs, outcomes or command output. The console routes are
//! undocumented; shapes were verified live on 2026-10-21.
use anyhow::{Context, Result, bail, ensure};
use bubblepaw_core::{QuotaReading, QuotaSource};
use chrono::{DateTime, Duration, Utc};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    future::Future,
    io::Write,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration as StdDuration,
};
use tokio::sync::Mutex;

use crate::quota_sources::{UsagePollOutcome, UsagePollStatus};

pub const CONSOLE_URL: &str = "https://opencode.ai/console/";
const CLIENT_ID: &str = "opencode-cli";
const MAX_BODY: usize = 1_000_000;

/// Cloudflare rejects default library user agents. Send an honest one; never
/// imitate a browser or the opencode CLI.
pub fn user_agent() -> String {
    format!("bubblepaw/{}", env!("CARGO_PKG_VERSION"))
}

fn safe_name(name: &str) -> bool {
    (1..=100).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
        && !name.starts_with('.')
}

/// `<data dir>/auth/opencode-console-<account>.json`.
pub fn credential_path(data_dir: &Path, account: &str) -> Result<PathBuf> {
    ensure!(
        safe_name(account),
        "OpenCode login account names use letters, digits, '.', '_' and '-'"
    );
    Ok(data_dir
        .join("auth")
        .join(format!("opencode-console-{account}.json")))
}

/// A BubblePaw-owned console session. `Debug` never shows the tokens.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsoleCredential {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: DateTime<Utc>,
    pub org_id: String,
}
impl std::fmt::Debug for ConsoleCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsoleCredential")
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

fn token_text(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|text| !text.is_empty() && text.len() <= 16_384 && !text.contains(['\r', '\n']))
        .map(str::to_owned)
}

/// Token responses: device grant and refresh grant. A refresh may omit the
/// organization or a rotated refresh token; the previous values then stay.
fn credential_from(
    body: &Value,
    now: DateTime<Utc>,
    previous: Option<&ConsoleCredential>,
) -> Option<ConsoleCredential> {
    let access_token = token_text(&body["access_token"])?;
    let refresh_token = token_text(&body["refresh_token"])
        .or_else(|| previous.map(|old| old.refresh_token.clone()))?;
    let org_id = body["org_id"]
        .as_str()
        .filter(|org| safe_header(org))
        .map(str::to_owned)
        .or_else(|| previous.map(|old| old.org_id.clone()))?;
    let seconds = body["expires_in"]
        .as_i64()
        .or_else(|| body["expires_in"].as_f64().map(|value| value as i64))
        .filter(|seconds| *seconds > 0)
        .unwrap_or(3600)
        .min(366 * 24 * 3600);
    Some(ConsoleCredential {
        access_token,
        refresh_token,
        expires_at: now + Duration::seconds(seconds),
        org_id,
    })
}

fn safe_header(value: &str) -> bool {
    (1..=200).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_graphic())
}

/// Reads at most 1 MB; a missing, oversized or malformed file is no credential.
pub fn read_credential(path: &Path) -> Option<(ConsoleCredential, [u8; 32])> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_BODY as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_BODY {
        return None;
    }
    let credential: ConsoleCredential = serde_json::from_slice(&bytes).ok()?;
    let valid = token_text(&Value::from(credential.access_token.as_str())).is_some()
        && token_text(&Value::from(credential.refresh_token.as_str())).is_some()
        && safe_header(&credential.org_id);
    valid.then(|| (credential, Sha256::digest(&bytes).into()))
}

/// Replaces the file atomically (temporary file in the same directory, then
/// rename), user-only on Unix. Returns the fingerprint of what was written.
pub fn write_credential(path: &Path, credential: &ConsoleCredential) -> Result<[u8; 32]> {
    let directory = path.parent().context("credential path has no directory")?;
    std::fs::create_dir_all(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    }
    let bytes = serde_json::to_vec_pretty(credential)?;
    // NamedTempFile is created 0600 on Unix.
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(&bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)
        .map_err(|error| anyhow::anyhow!("could not store the OpenCode login: {}", error.error))?;
    Ok(Sha256::digest(&bytes).into())
}

/// Returns whether a stored login existed.
pub fn delete_credential(path: &Path) -> Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub type Sleeper =
    Arc<dyn Fn(StdDuration) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

pub fn tokio_sleeper() -> Sleeper {
    Arc::new(|delay| -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(tokio::time::sleep(delay))
    })
}

enum Fetch {
    Ok(Value),
    Unauthorized,
    Failed,
}

#[derive(Debug, PartialEq)]
enum RefreshError {
    SignedOut,
    Failed,
}

#[derive(Clone)]
pub struct ConsoleClient {
    base: Url,
    http: reqwest::Client,
}

impl ConsoleClient {
    pub fn new() -> Result<Self> {
        Self::with_base_url(CONSOLE_URL)
    }

    /// Tests point this at a local fake server.
    pub fn with_base_url(base: &str) -> Result<Self> {
        let base = if base.ends_with('/') {
            base.to_owned()
        } else {
            format!("{base}/")
        };
        Ok(Self {
            base: Url::parse(&base)?,
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(StdDuration::from_secs(15))
                .user_agent(user_agent())
                .build()?,
        })
    }

    async fn body(mut response: reqwest::Response) -> Option<Value> {
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if body.len() + chunk.len() > MAX_BODY {
                return None;
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).ok()
    }

    async fn post(&self, path: &str, body: &Value) -> Option<(u16, Value)> {
        let response = self
            .http
            .post(self.base.join(path).ok()?)
            .json(body)
            .send()
            .await
            .ok()?;
        let status = response.status().as_u16();
        Some((status, Self::body(response).await.unwrap_or(Value::Null)))
    }

    async fn get(&self, path: &str, credential: &ConsoleCredential) -> Fetch {
        let Ok(url) = self.base.join(path) else {
            return Fetch::Failed;
        };
        let Ok(response) = self
            .http
            .get(url)
            .bearer_auth(&credential.access_token)
            .header("x-org-id", &credential.org_id)
            .send()
            .await
        else {
            return Fetch::Failed;
        };
        match response.status().as_u16() {
            401 => Fetch::Unauthorized,
            200..=299 => Self::body(response).await.map_or(Fetch::Failed, Fetch::Ok),
            _ => Fetch::Failed,
        }
    }

    async fn refresh(
        &self,
        credential: &ConsoleCredential,
        now: DateTime<Utc>,
    ) -> Result<ConsoleCredential, RefreshError> {
        let (status, body) = self
            .post(
                "auth/device/token",
                &json!({"grant_type":"refresh_token","refresh_token":credential.refresh_token,"client_id":CLIENT_ID}),
            )
            .await
            .ok_or(RefreshError::Failed)?;
        if body["error"] == "invalid_grant" {
            return Err(RefreshError::SignedOut);
        }
        if !(200..300).contains(&status) {
            return Err(RefreshError::Failed);
        }
        credential_from(&body, now, Some(credential)).ok_or(RefreshError::Failed)
    }

    /// The opencode CLI's device flow. Prints only the verification URL and
    /// user code; the returned credential is for the caller to store.
    pub async fn device_login(
        &self,
        out: &mut dyn Write,
        sleep: &Sleeper,
    ) -> Result<ConsoleCredential> {
        let (status, code) = self
            .post(
                "auth/device/code",
                &json!({"client_id":CLIENT_ID,"supports_org_scope":true}),
            )
            .await
            .context("could not reach the OpenCode Console")?;
        ensure!(
            (200..300).contains(&status),
            "the OpenCode Console refused to start a login (HTTP {status})"
        );
        let device_code =
            token_text(&code["device_code"]).context("the OpenCode Console sent no device code")?;
        let user_code = code["user_code"]
            .as_str()
            .filter(|code| safe_header(code))
            .context("the OpenCode Console sent no user code")?;
        let verification = code["verification_uri_complete"]
            .as_str()
            .or_else(|| code["verification_uri"].as_str())
            .and_then(|link| self.base.join(link).ok())
            .filter(|link| matches!(link.scheme(), "https" | "http"))
            .context("the OpenCode Console sent no verification link")?;
        let mut interval = code["interval"]
            .as_u64()
            .filter(|seconds| (1..=60).contains(seconds))
            .unwrap_or(5);
        let lifetime = code["expires_in"].as_u64().filter(|seconds| *seconds > 0);
        writeln!(
            out,
            "Open {verification} and confirm the code {user_code}.\nWaiting for approval..."
        )?;
        out.flush()?;
        let expired = "the login code expired; run `bubblepaw login opencode` again";
        let mut waited = 0;
        loop {
            if lifetime.is_some_and(|lifetime| waited >= lifetime) {
                bail!(expired);
            }
            sleep(StdDuration::from_secs(interval)).await;
            waited += interval;
            let started = Utc::now();
            let Some((status, body)) = self
                .post(
                    "auth/device/token",
                    &json!({"grant_type":"urn:ietf:params:oauth:grant-type:device_code","device_code":device_code,"client_id":CLIENT_ID}),
                )
                .await
            else {
                bail!("lost contact with the OpenCode Console during login; run `bubblepaw login opencode` again");
            };
            if (200..300).contains(&status) {
                return credential_from(&body, started, None).context(
                    "the OpenCode Console sent an incomplete login (no token or organization)",
                );
            }
            match body["error"].as_str() {
                Some("authorization_pending") => {}
                Some("slow_down") => interval += 5,
                Some("expired_token") => bail!(expired),
                Some("access_denied") => bail!("the login was declined in the browser"),
                _ => bail!("the OpenCode Console rejected the login (HTTP {status})"),
            }
        }
    }
}

/// Decimal micro-cent strings (numbers are accepted too).
fn micro(value: &Value) -> Option<i128> {
    match value {
        Value::String(text) => text.trim().parse().ok(),
        Value::Number(number) => number.as_i64().map(i128::from),
        _ => None,
    }
}

/// One micro-cent is 1e-8 dollars. Truncates to whole cents.
fn dollars(micro_cents: i128) -> String {
    let sign = if micro_cents < 0 { "-" } else { "" };
    let amount = micro_cents.unsigned_abs();
    format!(
        "{sign}{}.{:02}",
        amount / 100_000_000,
        (amount % 100_000_000) / 1_000_000
    )
}

fn reset_time(value: &Value) -> Option<String> {
    let at = match value {
        Value::String(text) => DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|at| at.with_timezone(&Utc)),
        Value::Number(number) => {
            let raw = number.as_i64()?;
            if raw > 100_000_000_000 {
                DateTime::from_timestamp_millis(raw)
            } else {
                DateTime::from_timestamp(raw, 0)
            }
        }
        _ => None,
    }?;
    Some(at.to_rfc3339())
}

fn reading(
    provider: &str,
    account: &str,
    window: &str,
    used: f64,
    resets_at: Option<String>,
    now: DateTime<Utc>,
) -> Option<QuotaReading> {
    if !used.is_finite() {
        return None;
    }
    let reading = QuotaReading {
        provider: provider.into(),
        account: account.into(),
        window: window.into(),
        used_percent: used.clamp(0.0, 100.0),
        resets_at,
        observed_at: now.to_rfc3339(),
        source: QuotaSource::UsageApi,
        balance: None,
    };
    reading.validate().ok().map(|()| reading)
}

/// `api/go/status` meters as `five_hour`, `week` and `month`, plus the
/// `credits` window from `api/billing/status` when that answered. Only typed
/// numeric fields survive; a meter without a positive limit is unknown.
pub fn parse_go_status(
    provider: &str,
    account: &str,
    go: &Value,
    billing: Option<&Value>,
    now: DateTime<Utc>,
) -> Vec<QuotaReading> {
    let meters = &go["access"]["meters"];
    let mut readings: Vec<_> = [
        ("fiveHour", "five_hour"),
        ("week", "week"),
        ("month", "month"),
    ]
    .into_iter()
    .filter_map(|(field, window)| {
        let meter = &meters[field];
        let limit = micro(&meter["limitMicroCents"]).filter(|limit| *limit > 0)?;
        let used = micro(&meter["usedMicroCents"]).filter(|used| *used >= 0)?;
        reading(
            provider,
            account,
            window,
            100.0 * used as f64 / limit as f64,
            reset_time(&meter["resetsAt"]),
            now,
        )
    })
    .collect();
    if let Some(billing) = billing
        && let Some(available) = micro(&billing["availableMicroCents"])
    {
        let use_balance = go["useBalance"].as_bool().unwrap_or(false);
        let used = if use_balance && available > 0 {
            0.0
        } else {
            100.0
        };
        let balance = micro(&billing["balanceMicroCents"]).unwrap_or(available);
        if let Some(mut credits) = reading(provider, account, "credits", used, None, now) {
            credits.balance = Some(dollars(balance));
            readings.push(credits);
        }
    }
    readings
}

#[derive(Default)]
struct PollState {
    fingerprint: Option<[u8; 32]>,
    signed_out: bool,
    unauthorized: bool,
    last_attempt: Option<DateTime<Utc>>,
    next_attempt: Option<DateTime<Utc>>,
    last_success: Option<DateTime<Utc>>,
    failures: u32,
}

/// One shared instance per OpenCode account. Same policy as the Claude usage
/// API: on demand, stale after five minutes, at most one attempt per 60 s,
/// backoff on failure, no background polling. No stored login means no source.
pub struct OpenCodeUsagePoller {
    provider: String,
    account: String,
    path: PathBuf,
    client: ConsoleClient,
    state: Mutex<PollState>,
}

impl OpenCodeUsagePoller {
    pub fn new(
        provider: impl Into<String>,
        account: impl Into<String>,
        credential_path: impl AsRef<Path>,
        client: ConsoleClient,
    ) -> Result<Self> {
        let provider = provider.into();
        let account = account.into();
        ensure!(
            !provider.trim().is_empty() && !account.trim().is_empty(),
            "quota poller needs provider and account"
        );
        Ok(Self {
            provider,
            account,
            path: credential_path.as_ref().to_owned(),
            client,
            state: Mutex::default(),
        })
    }

    pub fn account(&self) -> &str {
        &self.account
    }

    fn login_message(&self) -> String {
        format!(
            "OpenCode quota for account {0} is paused: the console login expired or was revoked; run `bubblepaw login opencode --account {0}`",
            self.account
        )
    }

    fn paused(&self, status: UsagePollStatus, message: bool) -> UsagePollOutcome {
        UsagePollOutcome {
            status,
            readings: Vec::new(),
            http_status: None,
            message: message.then(|| self.login_message()),
        }
    }

    fn failed(&self, state: &mut PollState, now: DateTime<Utc>) -> UsagePollOutcome {
        state.failures = state.failures.saturating_add(1).min(32);
        let delay = 300_i64
            .saturating_mul(1_i64 << (state.failures - 1).min(10))
            .min(1800);
        state.next_attempt = now.checked_add_signed(Duration::seconds(delay));
        self.paused(UsagePollStatus::Backoff, false)
    }

    /// Refreshes under a per-account lock file so two BubblePaw processes never
    /// spend the same refresh token. A login rotated meanwhile is adopted.
    async fn refresh(
        &self,
        state: &mut PollState,
        credential: &ConsoleCredential,
        now: DateTime<Utc>,
    ) -> Result<ConsoleCredential, RefreshError> {
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.path.with_extension("lock"))
            .map_err(|_| RefreshError::Failed)?;
        fs2::FileExt::try_lock_exclusive(&lock).map_err(|_| RefreshError::Failed)?;
        if let Some((current, fingerprint)) = read_credential(&self.path)
            && current.refresh_token != credential.refresh_token
            && current.expires_at > now + Duration::seconds(60)
        {
            state.fingerprint = Some(fingerprint);
            return Ok(current);
        }
        let rotated = self.client.refresh(credential, now).await?;
        match write_credential(&self.path, &rotated) {
            Ok(fingerprint) => state.fingerprint = Some(fingerprint),
            Err(error) => {
                tracing::warn!(account = %self.account, %error, "rotated OpenCode login could not be stored")
            }
        }
        drop(lock);
        Ok(rotated)
    }

    /// `force` bypasses freshness only; a 429-triggered poll still respects the
    /// minimum cadence, backoff and the signed-out pause.
    pub async fn poll_if_stale(
        &self,
        now: DateTime<Utc>,
        latest_observed_at: Option<&str>,
        force: bool,
    ) -> UsagePollOutcome {
        let mut state = self.state.lock().await;
        let Some((mut credential, fingerprint)) = read_credential(&self.path) else {
            return self.paused(UsagePollStatus::CredentialsUnavailable, false);
        };
        if state.fingerprint != Some(fingerprint) {
            *state = PollState {
                fingerprint: Some(fingerprint),
                last_attempt: state.last_attempt,
                last_success: state.last_success,
                ..PollState::default()
            };
        }
        if state.signed_out {
            return self.paused(UsagePollStatus::LoginExpired, false);
        }
        if state.unauthorized {
            return self.paused(UsagePollStatus::Unauthorized, false);
        }
        if state
            .last_attempt
            .is_some_and(|last| now < last + Duration::seconds(60))
        {
            return self.paused(UsagePollStatus::Floor, false);
        }
        if state.next_attempt.is_some_and(|next| now < next) {
            return self.paused(UsagePollStatus::Backoff, false);
        }
        let newest = latest_observed_at
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.with_timezone(&Utc))
            .into_iter()
            .chain(state.last_success)
            .max();
        if !force && newest.is_some_and(|at| now < at + Duration::minutes(5)) {
            return self.paused(UsagePollStatus::Fresh, false);
        }
        state.last_attempt = Some(now);
        let mut refreshed = false;
        if credential.expires_at <= now + Duration::seconds(60) {
            match self.refresh(&mut state, &credential, now).await {
                Ok(rotated) => {
                    credential = rotated;
                    refreshed = true;
                }
                Err(RefreshError::SignedOut) => {
                    state.signed_out = true;
                    return self.paused(UsagePollStatus::LoginExpired, true);
                }
                Err(RefreshError::Failed) => return self.failed(&mut state, now),
            }
        }
        let mut go = self.client.get("api/go/status", &credential).await;
        if matches!(go, Fetch::Unauthorized) && !refreshed {
            match self.refresh(&mut state, &credential, now).await {
                Ok(rotated) => {
                    credential = rotated;
                    go = self.client.get("api/go/status", &credential).await;
                }
                Err(RefreshError::SignedOut) => {
                    state.signed_out = true;
                    return self.paused(UsagePollStatus::LoginExpired, true);
                }
                Err(RefreshError::Failed) => return self.failed(&mut state, now),
            }
        }
        let go = match go {
            Fetch::Ok(go) => go,
            Fetch::Unauthorized => {
                // A fresh token was refused too: pause until the login changes.
                state.unauthorized = true;
                let mut outcome = self.paused(UsagePollStatus::Unauthorized, true);
                outcome.http_status = Some(401);
                return outcome;
            }
            Fetch::Failed => return self.failed(&mut state, now),
        };
        // Billing is best effort: the meters stand on their own.
        let billing = match self.client.get("api/billing/status", &credential).await {
            Fetch::Ok(billing) => Some(billing),
            _ => None,
        };
        let mut readings =
            parse_go_status(&self.provider, &self.account, &go, billing.as_ref(), now);
        readings.retain(|reading| {
            !reading.window.contains(&credential.access_token)
                && reading
                    .balance
                    .as_deref()
                    .is_none_or(|balance| !balance.contains(&credential.access_token))
        });
        if readings.is_empty() {
            return self.failed(&mut state, now);
        }
        state.last_success = Some(now);
        state.failures = 0;
        state.next_attempt = None;
        UsagePollOutcome {
            status: UsagePollStatus::Ok,
            readings,
            http_status: Some(200),
            message: None,
        }
    }
}
