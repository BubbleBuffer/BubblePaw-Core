//! Typed decision providers. Hosts resolve model roles and perform repeats;
//! each provider invocation produces one sample.
use crate::{CancellationToken, ReasoningEffort, Usage};
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DecisionRequest {
    pub model: String,
    pub input: String,
    pub questions: Vec<Question>,
    pub session_id: Option<String>,
    #[serde(default)]
    pub reasoning_effort: ReasoningEffort,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Question {
    pub name: String,
    pub instructions: String,
    pub kind: QuestionKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QuestionKind {
    Predicate,
    Choice { choices: Vec<DecisionChoice> },
    Score { levels: Vec<ScoreLevel> },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DecisionChoice {
    pub value: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScoreLevel {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionResponse {
    pub answers: Vec<Answer>,
    pub model: String,
    #[serde(default)]
    pub usage: DecisionUsage,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Answer {
    pub name: String,
    pub kind: AnswerKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AnswerKind {
    Predicate {
        probability: f64,
    },
    Choice {
        choice: String,
        probabilities: Vec<(String, f64)>,
    },
    Score {
        score: f64,
        probabilities: Vec<(String, f64)>,
    },
    Refusal,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DecisionUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Prompt-cache tokens of chat-backed decisions; zero for adapters that
    /// do not report them.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_read_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_creation_tokens: u64,
}
fn is_zero(value: &u64) -> bool {
    *value == 0
}
impl DecisionUsage {
    pub fn add(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_creation_tokens = self
            .cache_creation_tokens
            .saturating_add(other.cache_creation_tokens);
        if let Some(cost) = other.cost_usd {
            self.cost_usd = Some(self.cost_usd.unwrap_or(0.0) + cost);
        }
    }
}
impl From<&Usage> for DecisionUsage {
    fn from(usage: &Usage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cost_usd: None,
            cache_read_tokens: usage.cache_read_tokens,
            cache_creation_tokens: usage.cache_creation_tokens,
        }
    }
}
impl From<Usage> for DecisionUsage {
    fn from(usage: Usage) -> Self {
        Self::from(&usage)
    }
}
impl From<DecisionUsage> for Usage {
    fn from(usage: DecisionUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_creation_tokens: usage.cache_creation_tokens,
            reasoning_tokens: None,
        }
    }
}

#[async_trait]
pub trait DecisionProvider: Send + Sync {
    async fn decide(
        &self,
        request: DecisionRequest,
        cancellation: CancellationToken,
    ) -> Result<DecisionResponse>;
}
