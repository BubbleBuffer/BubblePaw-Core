//! Account meters and rejection scheduling belong to the host, not the engine.
use anyhow::{Result, anyhow};
use bubblepaw_core::{
    CancellationToken, ModelResponse, ProviderError, ProviderErrorKind, QuotaReading, QuotaSource,
    RequestUsage, Store,
};
use chrono::{DateTime, Utc};
use std::{
    collections::BTreeMap,
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use crate::{
    config::Settings,
    opencode_console::{ConsoleClient, OpenCodeUsagePoller, credential_path},
    quota_sources::{ClaudeUsagePoller, UsagePollOutcome},
};

mod attribution;
use attribution::normalize_error;

pub const SHORT_WAIT: Duration = Duration::from_secs(15);
const SHORT_ATTEMPTS: usize = 4;
tokio::task_local! { static QUOTA_POLICY: (); }
/// Transport retries yield a 429 to the account policy before sleeping.
pub fn host_policy_active() -> bool {
    QUOTA_POLICY.try_with(|_| ()).is_ok()
}
/// Runs `future` as if the quota host drove it, for the retry rule's tests.
#[cfg(test)]
pub(crate) async fn with_host_policy<F: std::future::Future>(future: F) -> F::Output {
    QUOTA_POLICY.scope((), future).await
}

#[derive(Debug, PartialEq)]
pub enum LimitAction {
    Short(Duration),
    Park {
        resume_at: DateTime<Utc>,
        window: String,
    },
    Fail(String),
}

/// Classify only rejected, unpublished requests. Old windows whose reset has
/// passed remain useful history, but cannot justify another long wait.
pub fn classify_limit(
    account: &str,
    model: &str,
    error: &anyhow::Error,
    readings: &[QuotaReading],
    now: DateTime<Utc>,
    max_wait: Duration,
) -> LimitAction {
    let Some(typed) = ProviderError::of(error) else {
        return LimitAction::Fail(error.to_string());
    };
    if !typed.retryable() || !matches!(typed.kind, ProviderErrorKind::RateLimit { .. }) {
        return LimitAction::Fail(typed.message.clone());
    }
    // Only this rejection can name the window that rejected the request. A
    // previously observed exhausted week must not override its five-hour reset.
    let applies = |reading: &&QuotaReading| {
        reading.account == account && window_applies(&reading.window, model)
    };
    let current_count = crate::provider::current_rejection_quota_count(error)
        .unwrap_or(typed.quota.len())
        .min(typed.quota.len());
    let current = &typed.quota[..current_count];
    let rejected: Vec<_> = current
        .iter()
        .filter(|reading| {
            reading.account == account
                && reading.window.split(':').next() != Some("credits")
                && reading.source == QuotaSource::Rejection
                // `subscription` is the SDK bridge's placeholder when the
                // event named no window. A supplied reset is still evidence.
                && (reading.window != "subscription" || reading.resets_at.is_some())
        })
        .collect();
    let headers: Vec<_> = current
        .iter()
        .filter(applies)
        .filter(|reading| {
            reading.source == QuotaSource::ResponseHeaders && reading.used_percent >= 100.0
        })
        .collect();
    let named = if !rejected.is_empty() {
        rejected
    } else {
        headers
    };
    if !named.is_empty() {
        return classify_windows(account, &named, now, max_wait)
            .unwrap_or(LimitAction::Short(Duration::ZERO));
    }
    let reset = match &typed.kind {
        ProviderErrorKind::RateLimit { resets_at } => resets_at.as_deref().and_then(parsed_time),
        _ => None,
    };
    let hint = crate::provider::provider_retry_after(error);
    let reset = reset.or_else(|| {
        hint.and_then(|hint| chrono::Duration::from_std(hint).ok())
            .and_then(|delay| now.checked_add_signed(delay))
    });
    // A current Retry-After/reset also outranks historical quota observations.
    if let Some(at) = reset {
        let delay = at.signed_duration_since(now).to_std().unwrap_or_default();
        return if delay > SHORT_WAIT {
            quota_wait(account, "rate_limit", at, now, max_wait)
        } else {
            LimitAction::Short(delay)
        };
    }
    let fallback: Vec<_> = readings
        .iter()
        .filter(applies)
        .filter(|reading| reading.used_percent >= 100.0)
        .collect();
    // Without a named limit, try the first applicable reset. Another real 429
    // can then identify a longer limit, without guessing that it applies now.
    classify_windows(account, &fallback, now, max_wait)
        .unwrap_or(LimitAction::Short(hint.unwrap_or_default().min(SHORT_WAIT)))
}

fn classify_windows(
    account: &str,
    candidates: &[&QuotaReading],
    now: DateTime<Utc>,
    max_wait: Duration,
) -> Option<LimitAction> {
    if let Some((resume_at, reading)) = candidates
        .iter()
        .filter_map(|reading| {
            reading
                .resets_at
                .as_deref()
                .and_then(parsed_time)
                .filter(|at| *at > now)
                .map(|at| (at, *reading))
        })
        .min_by_key(|(at, _)| *at)
    {
        return Some(quota_wait(
            account,
            &reading.window,
            resume_at,
            now,
            max_wait,
        ));
    }
    if let Some(reading) = candidates
        .iter()
        .find(|reading| reading.resets_at.as_deref().and_then(parsed_time).is_none())
    {
        return Some(LimitAction::Fail(format!(
            "quota exhausted for account {account}, window {}; reset time unknown",
            reading.window
        )));
    }
    None
}

fn quota_wait(
    account: &str,
    window: &str,
    resume_at: DateTime<Utc>,
    now: DateTime<Utc>,
    max_wait: Duration,
) -> LimitAction {
    if resume_at
        .signed_duration_since(now)
        .to_std()
        .is_ok_and(|delay| delay > max_wait)
    {
        return LimitAction::Fail(format!(
            "quota exhausted until {} for account {account}, window {window}; maximum wait exceeded",
            resume_at.to_rfc3339()
        ));
    }
    LimitAction::Park {
        resume_at,
        window: window.into(),
    }
}

/// On-demand usage sources, one per account.
enum UsagePoller {
    Claude(ClaudeUsagePoller),
    OpenCode(OpenCodeUsagePoller),
}
impl UsagePoller {
    async fn poll_if_stale(
        &self,
        now: DateTime<Utc>,
        latest_observed_at: Option<&str>,
        force: bool,
    ) -> UsagePollOutcome {
        match self {
            Self::Claude(poller) => poller.poll_if_stale(now, latest_observed_at, force).await,
            Self::OpenCode(poller) => poller.poll_if_stale(now, latest_observed_at, force).await,
        }
    }
}

fn window_applies(window: &str, model: &str) -> bool {
    let (kind, scope) = window
        .split_once(':')
        .map_or((window, None), |(kind, scope)| (kind, Some(scope)));
    if let Some(family) = kind.strip_prefix("seven_day_") {
        return scope.is_none()
            && matches!(family, "sonnet" | "opus" | "haiku" | "fable")
            && model_family(model).as_deref() == Some(family);
    }
    let known = matches!(
        kind,
        "five_hour"
            | "week"
            | "month"
            | "seven_day"
            | "weekly_all"
            | "session"
            | "primary"
            | "secondary"
            | "requests"
            | "tokens"
            | "input_tokens"
            | "output_tokens"
    );
    if !known && kind != "weekly_scoped" {
        return false;
    }
    match scope {
        Some(scope) => model_family(scope)
            .zip(model_family(model))
            .is_some_and(|(scope, model)| scope == model),
        None => known,
    }
}

fn model_family(model: &str) -> Option<String> {
    let model = model.to_ascii_lowercase();
    let mut families = model
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|part| matches!(*part, "sonnet" | "opus" | "haiku" | "fable"));
    let family = families.next()?;
    families.next().is_none().then(|| family.to_owned())
}

/// Stops outer retry/fallback loops after the quota policy has made a terminal
/// decision. The typed provider error remains discoverable through its source.
#[derive(Debug)]
pub struct QuotaStop(pub ProviderError);
impl std::fmt::Display for QuotaStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for QuotaStop {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

pub fn is_quota_stop(error: &anyhow::Error) -> bool {
    error.downcast_ref::<QuotaStop>().is_some()
}

/// The freshest balance observed for one account. Change rows carry it; it
/// never causes a write by itself.
#[derive(Clone, Debug)]
struct AccountBalance {
    balance: String,
    observed_at: String,
}

pub struct QuotaHost {
    store: Store,
    max_wait: Duration,
    latest: Mutex<BTreeMap<(String, String), QuotaReading>>,
    balances: Mutex<BTreeMap<String, AccountBalance>>,
    pollers: BTreeMap<String, Arc<UsagePoller>>,
}

impl QuotaHost {
    pub fn from_settings(store: Store, settings: &Settings) -> Result<Self> {
        Self::from_settings_with_env(store, settings, &|name| std::env::var(name).ok())
    }
    pub fn from_settings_with_env(
        store: Store,
        settings: &Settings,
        environment: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        let mut host = Self::new(store, Duration::from_secs(settings.quota.max_wait_seconds))?;
        for extension in &settings.extensions {
            let claude = extension.env.contains_key("CLAUDE_CONFIG_DIR")
                || extension.id.starts_with("claude-code")
                || extension
                    .cwd
                    .as_ref()
                    .is_some_and(|path| path.to_string_lossy().contains("claude-code"))
                || extension.args.iter().any(|arg| arg.contains("claude-code"));
            if !claude
                || extension.env.contains_key("ANTHROPIC_API_KEY")
                || extension.env.contains_key("ANTHROPIC_AUTH_TOKEN")
                || extension.env.contains_key("CLAUDE_CODE_OAUTH_TOKEN")
                || environment("CLAUDE_CODE_OAUTH_TOKEN").is_some()
            {
                continue;
            }
            let account = extension.account.as_ref().unwrap_or(&extension.id).clone();
            if host.pollers.contains_key(&account) {
                continue;
            }
            let directory = extension
                .env
                .get("CLAUDE_CONFIG_DIR")
                .map(std::path::PathBuf::from)
                .or_else(|| environment("CLAUDE_CONFIG_DIR").map(std::path::PathBuf::from))
                .or_else(|| {
                    environment("HOME")
                        .or_else(|| environment("USERPROFILE"))
                        .map(|home| std::path::PathBuf::from(home).join(".claude"))
                });
            if let Some(mut directory) = directory {
                if directory.is_relative() {
                    directory = extension
                        .cwd
                        .clone()
                        .unwrap_or(std::env::current_dir()?)
                        .join(directory);
                }
                host.pollers.insert(
                    account.clone(),
                    Arc::new(UsagePoller::Claude(ClaudeUsagePoller::new(
                        extension.id.clone(),
                        account,
                        directory.join(".credentials.json"),
                    )?)),
                );
            }
        }
        // OpenCode routes read a BubblePaw-owned console login from the data
        // directory. The route is implicit when no `[providers.opencode]` exists.
        if let Some(data_dir) = host.store.data_dir().map(std::path::Path::to_path_buf) {
            let implicit = (!settings.providers.contains_key("opencode"))
                .then(|| ("opencode".to_owned(), "opencode".to_owned()));
            let routes = settings
                .providers
                .iter()
                .filter(|(name, _)| crate::config::is_opencode_route(name))
                .map(|(name, provider)| {
                    (
                        name.clone(),
                        provider.account.as_ref().unwrap_or(name).clone(),
                    )
                })
                .chain(implicit);
            for (name, account) in routes {
                if host.pollers.contains_key(&account) {
                    continue;
                }
                let Ok(path) = credential_path(&data_dir, &account) else {
                    continue;
                };
                let poller = OpenCodeUsagePoller::new(name, account, path, ConsoleClient::new()?)?;
                host = host.with_opencode_poller(poller);
            }
        }
        Ok(host)
    }

    pub fn new(store: Store, max_wait: Duration) -> Result<Self> {
        let latest = store
            .latest_quota()?
            .into_iter()
            .map(|row| ((row.account.clone(), row.window.clone()), row))
            .collect();
        // Resume from the newest stored balance so a reopened host keeps it.
        let balances = store
            .latest_quota_balances()?
            .into_iter()
            .filter_map(|row| {
                let balance = row.balance?;
                Some((
                    row.account,
                    AccountBalance {
                        balance,
                        observed_at: row.observed_at,
                    },
                ))
            })
            .collect();
        Ok(Self {
            store,
            max_wait,
            latest: Mutex::new(latest),
            balances: Mutex::new(balances),
            pollers: BTreeMap::new(),
        })
    }

    /// Adds an OpenCode Console source unless the account already has one.
    pub fn with_opencode_poller(mut self, poller: OpenCodeUsagePoller) -> Self {
        let account = poller.account().to_owned();
        self.pollers
            .entry(account)
            .or_insert_with(|| Arc::new(UsagePoller::OpenCode(poller)));
        self
    }

    /// Latest reading per window, each with the balance it actually carried.
    /// Use `latest_balance` for the account's freshest balance and its time.
    pub fn latest(&self, account: &str) -> Vec<QuotaReading> {
        self.latest
            .lock()
            .unwrap()
            .values()
            .filter(|row| row.account == account)
            .cloned()
            .collect()
    }

    /// The freshest known account balance and its observation time, in memory
    /// or restored from the last stored change row.
    pub fn latest_balance(&self, account: &str) -> Option<(String, String)> {
        self.balances
            .lock()
            .unwrap()
            .get(account)
            .map(|known| (known.balance.clone(), known.observed_at.clone()))
    }

    pub fn observe(&self, provider: &str, account: &str, readings: &[QuotaReading]) -> Result<()> {
        let mut latest = self.latest.lock().unwrap();
        let mut balances = self.balances.lock().unwrap();
        // Transports append earlier attempts after the current request's quota.
        // Apply each batch in observation order before rejecting late arrivals.
        let mut ordered: Vec<QuotaReading> = readings
            .iter()
            .map(|reading| {
                let mut reading = reading.clone();
                reading.provider = provider.to_owned();
                reading.account = account.to_owned();
                reading
            })
            .collect();
        for reading in &ordered {
            reading.validate()?;
        }
        ordered.sort_by_key(|reading| parsed_time(&reading.observed_at));
        // Transport batches can mix attempts with different times. Each observed
        // time is one group, applied oldest first, so a later group's balance
        // never reaches an earlier row. Balances are never written on their own:
        // an unchanged meter adds no transaction.
        for group in
            ordered.chunk_by(|a, b| parsed_time(&a.observed_at) == parsed_time(&b.observed_at))
        {
            // Adopt the group's newest balance before its meters, so a secondary
            // listed before credits at the same time still carries it.
            for reading in group {
                let Some(balance) = &reading.balance else {
                    continue;
                };
                let fresher = balances.get(account).is_none_or(|known| {
                    parsed_time(&known.observed_at) <= parsed_time(&reading.observed_at)
                });
                if fresher {
                    balances.insert(
                        account.to_owned(),
                        AccountBalance {
                            balance: balance.clone(),
                            observed_at: reading.observed_at.clone(),
                        },
                    );
                }
            }
            for reading in group {
                let key = (account.to_owned(), reading.window.clone());
                // Late completions must not replace a more recent account reading.
                if latest.get(&key).is_some_and(|old| {
                    parsed_time(&old.observed_at) > parsed_time(&reading.observed_at)
                }) {
                    continue;
                }
                // Piggyback the latest known account balance on this change.
                let mut row = reading.clone();
                row.balance = balances.get(account).map(|known| known.balance.clone());
                self.store.record_quota(&row)?;
                latest.insert(key, reading.clone());
            }
        }
        Ok(())
    }

    async fn poll(&self, provider: &str, account: &str, force: bool) -> Result<()> {
        let Some(poller) = self.pollers.get(account) else {
            return Ok(());
        };
        let last = self
            .latest(account)
            .into_iter()
            .min_by_key(|r| parsed_time(&r.observed_at))
            .map(|r| r.observed_at);
        let outcome = poller
            .poll_if_stale(Utc::now(), last.as_deref(), force)
            .await;
        if let Some(message) = &outcome.message {
            tracing::warn!(%account, "{message}");
            eprintln!("{message}");
        }
        self.observe(provider, account, &outcome.readings)
    }

    /// Every invocation is a physical rejected request or completed response.
    /// A parked future retains all caller state, including run budgets.
    pub async fn execute<F, Fut>(
        &self,
        provider: &str,
        account: &str,
        model: &str,
        cancellation: CancellationToken,
        mut call: F,
    ) -> Result<ModelResponse>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<ModelResponse>>,
    {
        if cancellation.is_cancelled() {
            return Err(anyhow!("cancelled"));
        }
        tokio::select! { biased;
            _ = cancellation.cancelled() => return Err(anyhow!("cancelled")),
            result = self.poll(provider, account, false) => result?,
        }
        let mut short_attempt = 0;
        loop {
            if cancellation.is_cancelled() {
                return Err(anyhow!("cancelled"));
            }
            let mut record = RequestUsage::new(provider, model);
            let before = bubblepaw_core::request_usage_count();
            let result = QUOTA_POLICY.scope((), call()).await;
            if before == bubblepaw_core::request_usage_count() {
                record.failed = result.is_err();
                record.account = account.to_owned();
                record.usage = result
                    .as_ref()
                    .ok()
                    .and_then(|r| r.usage.clone())
                    .or_else(|| {
                        result
                            .as_ref()
                            .err()
                            .and_then(ProviderError::of)
                            .and_then(|e| e.usage.clone())
                    })
                    .unwrap_or_default();
                bubblepaw_core::record_request_usage(record);
            }
            match result {
                Ok(mut response) => {
                    self.observe(provider, account, &response.quota)?;
                    for reading in &mut response.quota {
                        reading.provider = provider.into();
                        reading.account = account.into();
                    }
                    return Ok(response);
                }
                Err(error) => {
                    let error = normalize_error(error, provider, account);
                    let Some(typed) = ProviderError::of(&error) else {
                        return Err(error);
                    };
                    self.observe(provider, account, &typed.quota)?;
                    if crate::provider::is_terminal_quota_error(typed) {
                        return Err(QuotaStop(typed.clone()).into());
                    }
                    if !matches!(typed.kind, ProviderErrorKind::RateLimit { .. }) {
                        return Err(error);
                    }
                    if typed.published {
                        return Err(error);
                    }
                    tokio::select! { biased;
                        _ = cancellation.cancelled() => return Err(anyhow!("cancelled")),
                        result = self.poll(provider, account, true) => result?,
                    }
                    match classify_limit(
                        account,
                        model,
                        &error,
                        &self.latest(account),
                        Utc::now(),
                        self.max_wait,
                    ) {
                        LimitAction::Fail(message) => {
                            let mut stop = typed.clone();
                            stop.kind = ProviderErrorKind::Fatal;
                            stop.message = message;
                            return Err(QuotaStop(stop).into());
                        }
                        LimitAction::Short(hint) => {
                            short_attempt += 1;
                            if short_attempt >= SHORT_ATTEMPTS {
                                return Err(error);
                            }
                            let delay = hint
                                .max(Duration::from_millis(500 * (1 << (short_attempt - 1))))
                                .min(SHORT_WAIT);
                            tokio::select! { biased; _ = cancellation.cancelled() => return Err(anyhow!("cancelled")), _ = tokio::time::sleep(delay) => {} }
                        }
                        LimitAction::Park { resume_at, window } => {
                            let reason = format!(
                                "quota exhausted until {} for account {account}, window {window}",
                                resume_at.to_rfc3339()
                            );
                            bubblepaw_core::set_quota_wait(
                                account,
                                &window,
                                &resume_at.to_rfc3339(),
                                &reason,
                            )?;
                            let delay = resume_at
                                .signed_duration_since(Utc::now())
                                .to_std()
                                .unwrap_or_default();
                            let cancelled = tokio::select! { biased; _ = cancellation.cancelled() => true, _ = tokio::time::sleep(delay) => false };
                            if cancelled {
                                return Err(anyhow!("cancelled"));
                            }
                            bubblepaw_core::clear_current_quota_wait()?;
                        }
                    }
                }
            }
        }
    }
}

fn parsed_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}
