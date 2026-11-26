//! Provider observations, independent of billing estimates and host routing policy.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QuotaSource {
    ResponseHeaders,
    SdkEvent,
    UsageApi,
    Rejection,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct QuotaReading {
    pub provider: String,
    pub account: String,
    pub window: String,
    pub used_percent: f64,
    pub resets_at: Option<String>,
    pub observed_at: String,
    pub source: QuotaSource,
    /// Account credit balance known when this reading was recorded. Stored only
    /// as a column on change rows; a balance never writes on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance: Option<String>,
}

fn utc(value: &str) -> Result<()> {
    let time = chrono::DateTime::parse_from_rfc3339(value)?;
    ensure!(
        time.offset().local_minus_utc() == 0,
        "Timestamp must use UTC"
    );
    Ok(())
}

impl QuotaReading {
    pub fn validate(&self) -> Result<()> {
        for identity in [&self.provider, &self.account, &self.window] {
            ensure!(
                !identity.trim().is_empty() && identity.len() <= 256,
                "Quota identity must contain 1..=256 bytes"
            );
        }
        ensure!(
            self.used_percent.is_finite() && (0.0..=100.0).contains(&self.used_percent),
            "Quota used_percent must be finite and in 0..=100"
        );
        utc(&self.observed_at)?;
        if let Some(reset) = &self.resets_at {
            utc(reset)?;
        }
        Ok(())
    }
}

/// A restart hint, not a serialized continuation. In-process waits keep the
/// existing future; restart recovery admits an ordinary new run from history.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuotaWait {
    pub session_id: String,
    pub run_id: String,
    pub account: String,
    pub window: String,
    pub resume_at: String,
    pub reason: String,
}

impl QuotaWait {
    pub fn validate(&self) -> Result<()> {
        for identity in [&self.session_id, &self.run_id, &self.account, &self.window] {
            ensure!(
                !identity.trim().is_empty() && identity.len() <= 256,
                "Quota wait identity must contain 1..=256 bytes"
            );
        }
        ensure!(
            !self.reason.trim().is_empty() && self.reason.len() <= 4096,
            "Quota wait reason must contain 1..=4096 bytes"
        );
        utc(&self.resume_at)
    }
}
