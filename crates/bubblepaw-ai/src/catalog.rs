//! The one model catalog: what the code knows per model name.
//!
//! The data lives in `catalog/models.json`, keyed by model name: list prices, the
//! largest synchronous output, and the context window where one is recorded. Each
//! fact keeps the matching rule its old source used, so the rules are not unified:
//! prices match the name after its provider prefix exactly and ignoring case
//! ([`PriceTable`]), the output ceiling matches it exactly and case-sensitively
//! ([`lookup`]), and the cache lifetime matches the provider string or a model-name
//! prefix ([`cache_lifetime`]). Facts that hold for a whole provider family rather
//! than one model are constants here, beside the data.
use std::time::Duration;

mod price;

pub use price::{Catalog, Rates, TokenClasses, instant};

/// The catalog under the name its price readers know it by.
pub type PriceTable = Catalog;

/// The catalog file as embedded, for fixtures that extend it.
pub const EMBEDDED_JSON: &str = include_str!("catalog/models.json");

/// Prefixes shorter than this are not cached (Anthropic's smallest minimum).
pub const MIN_CACHEABLE_TOKENS: u64 = 1024;
/// Anthropic's default ephemeral cache lifetime; no adapter requests 1 h.
pub const ANTHROPIC_CACHE_LIFETIME: Duration = Duration::from_secs(300);
/// OpenCode Go reuses cached input in 64-token blocks.
pub const DEFAULT_CACHE_BLOCK_TOKENS: u64 = 64;

/// What the catalog records for one model name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelFacts {
    /// Largest synchronous output a model accepts, used when a role sets no
    /// `max_output_tokens`. Unknown models get the default, 64000, which every
    /// listed model accepts; the provider decides whether to accept them.
    pub max_output_tokens: u32,
    /// Context window in tokens; recorded for no model yet and read by no adapter.
    pub context_window: Option<u64>,
}

/// Facts for a model name; the part after the last `/` is matched exactly.
pub fn lookup(model: &str) -> ModelFacts {
    let catalog = Catalog::shared();
    ModelFacts {
        max_output_tokens: catalog.max_output_tokens(model),
        context_window: catalog.context_window(model),
    }
}

/// Known provider cache lifetimes. Anthropic's is 5 minutes unless 1 h is requested.
pub fn cache_lifetime(provider: &str, model: &str) -> Option<Duration> {
    (provider.contains("anthropic") || model.starts_with("claude"))
        .then_some(ANTHROPIC_CACHE_LIFETIME)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins every answer to what the tables and matches it replaced said, so a data
    /// edit that changes one by accident fails here.
    #[test]
    fn answers_match_the_sources_they_replaced() {
        let ceilings = [
            ("claude-haiku-5-5", 128_000),
            ("claude/claude-haiku-5-5", 128_000),
            ("opencode-messages/claude-haiku-5-5", 128_000),
            ("Claude-Haiku-5-5", 64_000),
            ("claude-haiku-5-5-20261001", 64_000),
            ("claude-sonnet-5-5", 64_000),
            ("deepseek-v4.1-flash", 64_000),
            ("gpt-6-luna", 64_000),
            ("unknown", 64_000),
            ("", 64_000),
        ];
        for (model, ceiling) in ceilings {
            let facts = lookup(model);
            assert_eq!(facts.max_output_tokens, ceiling, "{model}");
            assert_eq!(facts.context_window, None, "{model}");
        }
        let lifetimes = [
            ("anthropic", "x", true),
            ("claude-code", "claude-haiku-5-5", true),
            ("opencode", "claude-haiku-5-5", true),
            ("opencode", "deepseek-v4.1-flash", false),
            ("openai", "gpt-6-luna", false),
            ("", "Claude-x", false),
            ("", "", false),
        ];
        for (provider, model, anthropic) in lifetimes {
            assert_eq!(
                cache_lifetime(provider, model),
                anthropic.then_some(Duration::from_secs(300)),
                "{provider}/{model}"
            );
        }
        assert_eq!(
            (MIN_CACHEABLE_TOKENS, DEFAULT_CACHE_BLOCK_TOKENS),
            (1024, 64)
        );
        let prices = Catalog::shared();
        assert_eq!(prices.checked, "2026-10-19");
        let priced = [
            ("opencode/deepseek-v4.1-flash", true),
            ("OpenCode/DeepSeek-V4.1-Flash", true),
            ("claude/claude-haiku-5-5", true),
            ("openai/gpt-6-luna", true),
            ("opencode/jev-1.13", false),
            ("deepseek-v4.1-flash-free", false),
            ("demo/unknown", false),
            ("", false),
        ];
        for (model, expected) in priced {
            assert_eq!(prices.is_priced(model), expected, "{model}");
        }
    }
}
