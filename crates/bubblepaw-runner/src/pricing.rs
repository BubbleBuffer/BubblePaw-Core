//! The one dated list-price table. Delegation ledger rows, usage exports and
//! session usage all price through it, at read time or when a ledger row is written.
//! A model is priced only when its name matches a listed ID exactly; unknown or
//! unpublished classes are unpriced, never estimated.
use bubblepaw_core::Usage;
use chrono::{DateTime, Datelike, Timelike, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::OnceLock};

const TABLE: &str = include_str!("prices.json");

#[derive(Debug, Deserialize)]
pub struct PriceTable {
    pub checked: String,
    models: BTreeMap<String, Entry>,
}
#[derive(Debug, Deserialize)]
struct Entry {
    billing: String,
    #[serde(default)]
    periods: BTreeMap<String, Rates>,
    peak_hours_utc: Option<PeakHours>,
    long_prompt: Option<LongPrompt>,
}
/// USD per million tokens for each billed class; None when the class is not published.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
pub struct Rates {
    pub uncached_input: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
    pub output: Option<f64>,
}
#[derive(Debug, Deserialize)]
struct PeakHours {
    weekdays: Vec<u32>,
    hours: Vec<(u32, u32)>,
}
#[derive(Debug, Deserialize)]
struct LongPrompt {
    over_tokens: u64,
    rates: Rates,
}

/// Token classes as billed: cache reads and writes are taken out of input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct TokenClasses {
    pub uncached_input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub reasoning: Option<u64>,
}
impl From<&Usage> for TokenClasses {
    fn from(usage: &Usage) -> Self {
        Self {
            uncached_input: usage
                .input_tokens
                .saturating_sub(usage.cache_read_tokens)
                .saturating_sub(usage.cache_creation_tokens),
            cache_read: usage.cache_read_tokens,
            cache_write: usage.cache_creation_tokens,
            output: usage.output_tokens,
            reasoning: usage.reasoning_tokens,
        }
    }
}
impl TokenClasses {
    /// None when cache tokens exceed the input they are subsets of: the record is inconsistent.
    pub fn checked(usage: &Usage) -> Option<Self> {
        let uncached_input = usage
            .input_tokens
            .checked_sub(usage.cache_read_tokens)?
            .checked_sub(usage.cache_creation_tokens)?;
        Some(Self {
            uncached_input,
            cache_read: usage.cache_read_tokens,
            cache_write: usage.cache_creation_tokens,
            output: usage.output_tokens,
            reasoning: usage.reasoning_tokens,
        })
    }
    pub fn input(&self) -> u64 {
        self.uncached_input + self.cache_read + self.cache_write
    }
    pub fn add(&mut self, other: &Self) {
        self.uncached_input += other.uncached_input;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.output += other.output;
        self.reasoning = match (self.reasoning, other.reasoning) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or_default() + b.unwrap_or_default()),
        };
    }
}

impl Rates {
    /// The one arithmetic for every price. A class with tokens needs a finite,
    /// nonnegative rate; a class with no tokens costs nothing.
    pub fn cost(&self, tokens: &TokenClasses) -> Option<f64> {
        let charge = |count: u64, rate: Option<f64>| match (count, rate) {
            (0, _) => Some(0.0),
            (n, Some(rate)) if rate.is_finite() && rate >= 0.0 => {
                Some(n as f64 * rate / 1_000_000.0)
            }
            _ => None,
        };
        // A null cache-write rate bills cache creation at the uncached input rate.
        let cost = charge(tokens.uncached_input, self.uncached_input)?
            + charge(tokens.cache_read, self.cache_read)?
            + charge(tokens.cache_write, self.cache_write.or(self.uncached_input))?
            + charge(tokens.output, self.output)?;
        cost.is_finite().then_some(cost)
    }
    /// The highest input-class rate, with cache creation at its effective rate.
    fn worst_input(&self) -> Option<f64> {
        let cache_write = self.cache_write.or(self.uncached_input)?;
        Some(self.uncached_input?.max(self.cache_read?).max(cache_write))
    }
}

/// Parse an RFC 3339 instant, as stored in records.
pub fn instant(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

impl PriceTable {
    pub fn embedded() -> Self {
        serde_json::from_str(TABLE).expect("embedded price table is valid")
    }
    pub fn from_json(text: &str) -> anyhow::Result<Self> {
        Ok(serde_json::from_str(text)?)
    }
    /// Whether a model name resolves to a list-priced entry.
    pub fn is_priced(&self, model: &str) -> bool {
        self.entry(model)
            .is_some_and(|entry| entry.billing == "list_price" && !entry.periods.is_empty())
    }
    /// Worst-case USD for a request with up to `input_tokens` input and `max_output`
    /// output tokens: the highest rates of any period, plus the long-prompt tier when
    /// `input_tokens` exceeds its threshold. None when the model is unpriced.
    pub fn reservation(&self, model: &str, input_tokens: u64, max_output: u64) -> Option<f64> {
        let entry = self.entry(model)?;
        if entry.billing != "list_price" {
            return None;
        }
        let mut tiers: Vec<Rates> = entry.periods.values().copied().collect();
        if let Some(long) = &entry.long_prompt
            && input_tokens > long.over_tokens
        {
            tiers.push(long.rates);
        }
        let mut input = 0.0_f64;
        let mut output = 0.0_f64;
        for rates in &tiers {
            input = input.max(rates.worst_input()?);
            output = output.max(rates.output?);
        }
        if tiers.is_empty() {
            return None;
        }
        Some((input_tokens as f64 * input + max_output as f64 * output) / 1_000_000.0)
    }
    /// The table used by every reader in this process; parsed once.
    pub fn shared() -> &'static Self {
        static SHARED: OnceLock<PriceTable> = OnceLock::new();
        SHARED.get_or_init(Self::embedded)
    }
    /// Peak or off-peak for this model at this time; peak when the model has peak hours
    /// and the time is unknown.
    pub fn period(&self, model: &str, at: Option<DateTime<Utc>>) -> &'static str {
        let Some(entry) = self.entry(model) else {
            return "off_peak";
        };
        match (&entry.peak_hours_utc, at) {
            (None, _) => "off_peak",
            (Some(_), None) => "peak",
            (Some(rule), Some(at)) => {
                let weekday = at.weekday().num_days_from_monday();
                let hour = at.hour();
                if rule.weekdays.contains(&weekday)
                    && rule.hours.iter().any(|(lo, hi)| (*lo..*hi).contains(&hour))
                {
                    "peak"
                } else {
                    "off_peak"
                }
            }
        }
    }
    /// Exact match on the model name after its provider prefix, ignoring case.
    /// `opencode/deepseek-v4.1-flash` matches; `deepseek-v4.1-flash-free` does not.
    fn entry(&self, model: &str) -> Option<&Entry> {
        let model = model.to_ascii_lowercase();
        let name = model.rsplit('/').next().unwrap_or(model.as_str());
        self.models.get(name)
    }
    /// USD for one call or a sum of calls; None when the model or a used class is unpriced.
    pub fn cost(
        &self,
        model: &str,
        tokens: &TokenClasses,
        at: Option<DateTime<Utc>>,
    ) -> Option<f64> {
        let entry = self.entry(model)?;
        if entry.billing != "list_price" {
            return None;
        }
        let period = self.period(model, at);
        let mut rates = *entry
            .periods
            .get(period)
            .or_else(|| entry.periods.get("off_peak"))?;
        if let Some(long) = &entry.long_prompt
            && tokens.input() > long.over_tokens
        {
            rates = long.rates;
        }
        rates.cost(tokens)
    }
    /// USD for one reported usage record; None when the record is inconsistent.
    pub fn usage_cost(&self, model: &str, usage: &Usage, at: Option<DateTime<Utc>>) -> Option<f64> {
        self.cost(model, &TokenClasses::checked(usage)?, at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn usage(input: u64, cache_read: u64, cache_write: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cache_read,
            cache_creation_tokens: cache_write,
            reasoning_tokens: None,
        }
    }

    #[test]
    fn prices_by_period_and_leaves_unpublished_classes_unpriced() {
        let table = PriceTable::embedded();
        assert_eq!(table.checked, "2026-10-19");
        let tokens = TokenClasses {
            uncached_input: 1_000_000,
            cache_read: 1_000_000,
            cache_write: 0,
            output: 1_000_000,
            reasoning: None,
        };
        // Monday 02:00 UTC is peak; Saturday is off-peak.
        let peak = Utc.with_ymd_and_hms(2026, 10, 19, 2, 0, 0).unwrap();
        let off = Utc.with_ymd_and_hms(2026, 10, 24, 2, 0, 0).unwrap();
        let flash = "opencode/deepseek-v4.1-flash";
        assert!((table.cost(flash, &tokens, Some(peak)).unwrap() - 1.506).abs() < 1e-9);
        assert!((table.cost(flash, &tokens, Some(off)).unwrap() - 0.753).abs() < 1e-9);
        assert_eq!(table.period(flash, None), "peak");
        let write = TokenClasses {
            cache_write: 5,
            ..tokens
        };
        // A null cache-write rate bills cache creation at the uncached input rate.
        let written = table.cost(flash, &write, Some(off)).unwrap();
        assert!(
            (written - (0.753 + 5.0 * 0.15 / 1e6)).abs() < 1e-12,
            "{written}"
        );
        assert_eq!(table.cost("opencode/jev-1.13", &tokens, None), None);
        assert_eq!(table.cost("demo/unknown", &tokens, None), None);
        let haiku = table
            .cost("claude/claude-haiku-5-5", &tokens, None)
            .unwrap();
        assert!((haiku - 3.05).abs() < 1e-9, "{haiku}");
        let classes = TokenClasses::from(&usage(10, 4, 1, 2));
        assert_eq!((classes.uncached_input, classes.input()), (5, 10));
    }

    #[test]
    fn model_names_match_exactly_after_the_provider_prefix() {
        let table = PriceTable::embedded();
        // Under the long-prompt threshold, so the base rates apply.
        let tokens = TokenClasses {
            uncached_input: 50_000,
            ..Default::default()
        };
        let off = Utc.with_ymd_and_hms(2026, 10, 24, 2, 0, 0).unwrap();
        let flash = table
            .cost("OpenCode/DeepSeek-V4.1-Flash", &tokens, Some(off))
            .unwrap();
        assert!((flash - 0.0075).abs() < 1e-12, "{flash}");
        let haiku = table
            .cost("opencode-messages/claude-haiku-5-5", &tokens, None)
            .unwrap();
        assert!((haiku - 0.005).abs() < 1e-12, "{haiku}");
        for guessed in [
            "free/deepseek-v4.1-flash-free",
            "deepseek-v4.1-flash-extra",
            "claude-haiku-5-5-20261001x",
            "flash",
            "opencode/flash",
            "claude-code/haiku",
            "",
        ] {
            assert_eq!(table.cost(guessed, &tokens, Some(off)), None, "{guessed}");
            assert_eq!(
                table.usage_cost(guessed, &usage(1, 0, 0, 0), None),
                None,
                "{guessed}"
            );
        }
    }

    #[test]
    fn inconsistent_usage_is_unpriced_not_clamped() {
        let table = PriceTable::embedded();
        let at = Utc.with_ymd_and_hms(2026, 10, 24, 2, 0, 0).unwrap();
        assert_eq!(
            table.usage_cost("claude-haiku-5-5", &usage(10, 8, 4, 0), Some(at)),
            None
        );
        let cost = table
            .usage_cost("claude-haiku-5-5", &usage(10, 8, 2, 0), Some(at))
            .unwrap();
        assert!(
            (cost - (8.0 * 0.01 + 2.0 * 0.125) / 1e6).abs() < 1e-15,
            "{cost}"
        );
    }

    #[test]
    fn reservation_is_the_worst_period_and_long_prompt_tier() {
        let table = PriceTable::embedded();
        let flash = "opencode/deepseek-v4.1-flash";
        // Peak input and output rates, whichever period is current.
        let flash_worst = table.reservation(flash, 1_000_000, 1_000_000).unwrap();
        assert!((flash_worst - 1.5).abs() < 1e-9, "{flash_worst}");
        // Haiku above 100k input reserves the 5x tier at its highest input class.
        let haiku = table
            .reservation("claude-haiku-5-5", 100_001, 65_536)
            .unwrap();
        let expected = (100_001.0 * 0.625 + 65_536.0 * 2.5) / 1e6;
        assert!((haiku - expected).abs() < 1e-12, "{haiku}");
        assert!(!table.is_priced("demo/unknown"));
        // Luna is a commonly configured decider, so its bare name must stay priced for replays.
        assert!(table.is_priced("openai/gpt-6-luna"));
        let luna = table
            .reservation("openai/gpt-6-luna", 1_000_000, 0)
            .unwrap();
        assert!((luna - 0.10).abs() < 1e-12, "{luna}");
        assert_eq!(table.reservation("demo/unknown", 1, 1), None);
    }

    #[test]
    fn shared_table_is_the_embedded_table() {
        assert_eq!(PriceTable::shared().checked, PriceTable::embedded().checked);
        assert_eq!(
            instant("2026-10-19T02:00:00+00:00"),
            Some(Utc.with_ymd_and_hms(2026, 10, 19, 2, 0, 0).unwrap())
        );
        assert_eq!(instant("not a time"), None);
    }
}
