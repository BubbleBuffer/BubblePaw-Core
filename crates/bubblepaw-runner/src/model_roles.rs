//! Model selection shared by episodes and approvals. Decider roles take repeated samples;
//! the approver makes one decision.
use crate::provider::ProviderRegistry;
use anyhow::{Result, ensure};
use async_trait::async_trait;
use bubblepaw_core::{
    Answer, AnswerKind, CancellationToken, DecisionProvider, DecisionRequest, DecisionResponse,
    DecisionUsage, QuestionKind, ReasoningEffort,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

pub type DecisionObserver = Arc<dyn Fn(&str, &Result<DecisionResponse>, Instant) + Send + Sync>;
#[derive(Clone, Debug)]
pub struct ModelRole {
    pub models: Vec<String>,
    pub reasoning_effort: ReasoningEffort,
    /// None uses the split calibration of the selected available model.
    pub repeats: Option<usize>,
    /// Independent override of the selected model's split threshold.
    pub threshold: Option<f64>,
    /// Output limit for each request of this role. None sends no limit (Messages uses its
    /// model ceiling); a length stop is continued either way.
    pub max_output_tokens: Option<u32>,
}
#[derive(Clone, Debug)]
pub struct ModelRoles {
    pub summary: ModelRole,
    pub chapter: ModelRole,
    pub decider: ModelRole,
    pub approver: ModelRole,
}
impl Default for ModelRoles {
    fn default() -> Self {
        Self::for_models("opencode/deepseek-v4.1-flash".into(), String::new())
    }
}
impl ModelRoles {
    pub fn validate(&self) -> Result<()> {
        // Decision roles may stay empty. Their first use fails with unconfigured_decision_role.
        ensure!(
            self.approver.repeats.is_none() && self.approver.threshold.is_none(),
            "roles.approver.repeats and roles.approver.threshold are not supported: the approver makes one yes/no decision; remove the key"
        );
        for role in [&self.summary, &self.chapter] {
            ensure!(!role.models.is_empty(), "Role requires at least one model");
        }
        for role in [&self.summary, &self.chapter, &self.decider, &self.approver] {
            ensure!(
                role.repeats.is_none_or(|repeats| repeats > 0),
                "Role repeats must be positive"
            );
            ensure!(
                role.threshold.is_none_or(
                    |threshold| threshold.is_finite() && (0.0..=1.0).contains(&threshold)
                ),
                "Role threshold must be between zero and one"
            );
            for model in &role.models {
                ensure!(
                    model.split_once('/').is_some_and(
                        |(prefix, name)| !prefix.trim().is_empty() && !name.trim().is_empty()
                    ),
                    "Role model must be qualified as <provider>/<model>: {model:?}"
                );
            }
        }
        for role in [&self.decider, &self.approver] {
            for model in &role.models {
                if let Some(llm) = model.strip_prefix("llm/") {
                    ensure!(
                        llm.split_once('/')
                            .is_some_and(|(provider, name)| !provider.trim().is_empty()
                                && !name.trim().is_empty()),
                        "LLM decision model must be qualified as llm/<provider>/<model>: {model:?}"
                    );
                }
            }
        }
        Ok(())
    }
    pub fn for_models(summary: String, main: String) -> Self {
        let mut summaries = vec![summary.clone()];
        if !main.is_empty() && !summaries.contains(&main) {
            summaries.push(main.clone());
        }
        // Decision roles have no built-in models. They must be configured explicitly.
        Self {
            summary: ModelRole {
                models: summaries.clone(),
                reasoning_effort: ReasoningEffort::Low,
                repeats: Some(1),
                threshold: Some(0.0),
                max_output_tokens: None,
            },
            chapter: ModelRole {
                models: summaries,
                reasoning_effort: ReasoningEffort::Low,
                repeats: Some(1),
                threshold: Some(0.0),
                max_output_tokens: None,
            },
            decider: ModelRole {
                models: Vec::new(),
                reasoning_effort: ReasoningEffort::Low,
                repeats: None,
                threshold: None,
                max_output_tokens: None,
            },
            approver: ModelRole {
                models: Vec::new(),
                reasoning_effort: ReasoningEffort::Low,
                repeats: None,
                threshold: None,
                max_output_tokens: None,
            },
        }
    }
}
impl ModelRole {
    /// Fails at a role's first use when no model is configured. Names the config key to set.
    /// Runs that never reach a decision never call this.
    pub fn require_configured(&self, key: &str) -> Result<()> {
        ensure!(
            !self.models.is_empty(),
            "No decision model is configured: set roles.{key} in the config to one or more <provider>/<model> entries (or llm/<provider>/<model>)"
        );
        Ok(())
    }
    /// Split calibration follows the selected model, not an unavailable primary.
    /// Explicit repeats and threshold override their defaults independently.
    pub fn decision_settings(&self, model: &str) -> (usize, f64) {
        let (repeats, threshold) = if model == "openai/gpt-6-luna" {
            (1, 0.6)
        } else {
            (3, 0.8)
        };
        (
            self.repeats.unwrap_or(repeats),
            self.threshold.unwrap_or(threshold),
        )
    }
    pub fn available_model(&self, registry: &ProviderRegistry) -> Option<&str> {
        self.models
            .iter()
            .find(|model| registry.decision_available(model))
            .map(String::as_str)
    }
    pub async fn decide(
        &self,
        registry: &ProviderRegistry,
        request: DecisionRequest,
        cancellation: CancellationToken,
    ) -> Result<DecisionResponse> {
        self.decide_with(registry, request, cancellation, Arc::new(|_, _, _| {}))
            .await
    }
    pub async fn decide_with(
        &self,
        registry: &ProviderRegistry,
        request: DecisionRequest,
        cancellation: CancellationToken,
        observer: DecisionObserver,
    ) -> Result<DecisionResponse> {
        self.decide_with_retry(
            registry,
            request,
            cancellation,
            Duration::from_millis(500),
            observer,
        )
        .await
    }
    pub async fn decide_with_retry(
        &self,
        registry: &ProviderRegistry,
        request: DecisionRequest,
        cancellation: CancellationToken,
        retry_delay: Duration,
        observer: DecisionObserver,
    ) -> Result<DecisionResponse> {
        self.decide_with_retry_reasoning(registry, request, cancellation, retry_delay, observer)
            .await
            .map(|(response, _)| response)
    }
    pub async fn decide_with_retry_reasoning(
        &self,
        registry: &ProviderRegistry,
        request: DecisionRequest,
        cancellation: CancellationToken,
        retry_delay: Duration,
        observer: DecisionObserver,
    ) -> Result<(DecisionResponse, Option<String>)> {
        self.decide_with_retry_selected(registry, request, cancellation, retry_delay, observer)
            .await
            .map(|(response, reasoning, _)| (response, reasoning))
    }
    pub async fn decide_with_retry_selected(
        &self,
        registry: &ProviderRegistry,
        request: DecisionRequest,
        cancellation: CancellationToken,
        retry_delay: Duration,
        observer: DecisionObserver,
    ) -> Result<(DecisionResponse, Option<String>, String)> {
        let session = request.session_id.clone().unwrap_or_default();
        registry
            .auxiliary_usage(
                &session,
                bubblepaw_core::RequestKind::Decider,
                self.decide_with_retry_selected_inner(
                    registry,
                    request,
                    cancellation,
                    retry_delay,
                    observer,
                ),
            )
            .await
    }
    async fn decide_with_retry_selected_inner(
        &self,
        registry: &ProviderRegistry,
        mut request: DecisionRequest,
        cancellation: CancellationToken,
        // Kept for the caller's contract; the registry owns the only retry loop.
        _retry_delay: Duration,
        observer: DecisionObserver,
    ) -> Result<(DecisionResponse, Option<String>, String)> {
        if cancellation.is_cancelled() {
            return Err(crate::provider::cancelled());
        }
        let mut total = DecisionUsage::default();
        let mut last_error = None;
        for model in self
            .models
            .iter()
            .filter(|model| registry.decision_available(model))
        {
            if cancellation.is_cancelled() {
                return Err(crate::provider::with_decision_usage(
                    crate::provider::cancelled(),
                    total,
                ));
            }
            let (repeats, _) = self.decision_settings(model);
            ensure!(repeats > 0, "Role repeats must be positive");
            request.model = model.clone();
            request.reasoning_effort = self.reasoning_effort;
            let mut responses = Vec::new();
            let mut reasoning = Vec::new();
            let mut failed = false;
            // Transport retries belong to the registry's policy; repeats are
            // independent samples, not retries.
            for _ in 0..repeats {
                let started = Instant::now();
                let (result, trace) = match registry
                    .decide_with_reasoning(request.clone(), cancellation.clone())
                    .await
                {
                    Ok((response, trace)) => (Ok(response), trace),
                    Err(error) => (Err(error), None),
                };
                observer(model, &result, started);
                match result {
                    Ok(response) => {
                        total.add(&response.usage);
                        responses.push(response);
                        if let Some(trace) = trace {
                            reasoning.push(trace);
                        }
                    }
                    Err(error) => {
                        if let Some(usage) = crate::provider::decision_error_usage(&error) {
                            total.add(usage);
                        } else if let Some(usage) = bubblepaw_core::ProviderError::of(&error)
                            .and_then(|error| error.usage.as_ref())
                        {
                            total.add(&DecisionUsage::from(usage));
                        }
                        let stopped = cancellation.is_cancelled()
                            || bubblepaw_core::ProviderError::of(&error).is_some_and(|error| {
                                error.published || error.message == "cancelled"
                            });
                        if stopped {
                            return Err(crate::provider::with_decision_usage(error, total));
                        }
                        last_error = Some(error);
                        failed = true;
                        break;
                    }
                }
            }
            // Do not mix partial samples from a failed model with a fallback's
            // probabilities or calibration. Their known usage still counts.
            if failed {
                continue;
            }
            let answers = request
                .questions
                .iter()
                .map(|question| {
                    let samples = responses
                        .iter()
                        .map(|response| {
                            response
                                .answers
                                .iter()
                                .find(|answer| answer.name == question.name)
                                .map(|answer| &answer.kind)
                                .unwrap_or(&AnswerKind::Refusal)
                        })
                        .collect::<Vec<_>>();
                    let kind = match &question.kind {
                        QuestionKind::Predicate => {
                            if samples
                                .iter()
                                .all(|sample| matches!(sample, AnswerKind::Predicate { .. }))
                            {
                                AnswerKind::Predicate {
                                    probability: samples
                                        .iter()
                                        .map(|sample| match sample {
                                            AnswerKind::Predicate { probability } => *probability,
                                            _ => 0.0,
                                        })
                                        .sum::<f64>()
                                        / repeats as f64,
                                }
                            } else {
                                AnswerKind::Refusal
                            }
                        }
                        QuestionKind::Choice { choices } => aggregate_labels(
                            &samples,
                            &choices
                                .iter()
                                .map(|choice| choice.value.clone())
                                .collect::<Vec<_>>(),
                            false,
                        ),
                        QuestionKind::Score { levels } => aggregate_labels(
                            &samples,
                            &levels
                                .iter()
                                .map(|level| level.label.clone())
                                .collect::<Vec<_>>(),
                            true,
                        ),
                    };
                    Answer {
                        name: question.name.clone(),
                        kind,
                    }
                })
                .collect();
            return Ok((
                DecisionResponse {
                    answers,
                    model: responses.last().unwrap().model.clone(),
                    usage: total,
                },
                (!reasoning.is_empty()).then(|| reasoning.join("\n\n")),
                model.to_owned(),
            ));
        }
        Err(crate::provider::with_decision_usage(
            last_error.unwrap_or_else(|| registry.no_decision_model(&self.models)),
            total,
        ))
    }
}
fn aggregate_labels(samples: &[&AnswerKind], labels: &[String], score: bool) -> AnswerKind {
    let mut probabilities: Vec<(String, f64)> =
        labels.iter().map(|label| (label.clone(), 0.0)).collect();
    for sample in samples {
        match sample {
            AnswerKind::Choice {
                probabilities: distribution,
                ..
            } if !score => {
                for (label, p) in &mut probabilities {
                    *p += distribution
                        .iter()
                        .find(|(value, _)| value == label)
                        .map(|(_, p)| *p)
                        .unwrap_or(0.0)
                        / samples.len() as f64;
                }
            }
            AnswerKind::Score {
                probabilities: distribution,
                ..
            } if score => {
                for (label, p) in &mut probabilities {
                    *p += distribution
                        .iter()
                        .find(|(value, _)| value == label)
                        .map(|(_, p)| *p)
                        .unwrap_or(0.0)
                        / samples.len() as f64;
                }
            }
            AnswerKind::Refusal if !score && labels.iter().any(|label| label == "none") => {
                probabilities
                    .iter_mut()
                    .find(|(label, _)| label == "none")
                    .unwrap()
                    .1 += 1.0 / samples.len() as f64;
            }
            _ => return AnswerKind::Refusal,
        }
    }
    let Some((choice, _)) = probabilities
        .iter()
        .enumerate()
        .max_by(|(i, a), (j, b)| a.1.total_cmp(&b.1).then_with(|| j.cmp(i)))
        .map(|(_, pair)| pair)
    else {
        return AnswerKind::Refusal;
    };
    let choice = choice.clone();
    if score {
        AnswerKind::Score {
            score: probabilities
                .iter()
                .enumerate()
                .map(|(index, (_, p))| index as f64 * p)
                .sum(),
            probabilities,
        }
    } else {
        AnswerKind::Choice {
            choice,
            probabilities,
        }
    }
}
/// The approver's prompt template: `approvals.auto_prompt`, or the built-in default.
pub fn approver_prompt(approvals: &crate::config::ApprovalSettings) -> String {
    approvals
        .auto_prompt
        .clone()
        .unwrap_or_else(|| bubblepaw_core::DEFAULT_AUTO_PROMPT.into())
}
/// The approval gate's auto-approval adapter, so it reads the `approver` role.
/// It makes one call on the first available model of the chain: no repeats.
pub struct RoleDecisionProvider {
    pub registry: Arc<ProviderRegistry>,
    pub role: ModelRole,
}
#[async_trait]
impl DecisionProvider for RoleDecisionProvider {
    async fn decide(
        &self,
        request: DecisionRequest,
        cancellation: CancellationToken,
    ) -> Result<DecisionResponse> {
        self.role.require_configured("approver")?;
        let role = ModelRole {
            repeats: Some(1),
            ..self.role.clone()
        };
        let (mut response, _, selected) = role
            .decide_with_retry_selected(
                &self.registry,
                request,
                cancellation,
                Duration::from_millis(500),
                Arc::new(|_, _, _| {}),
            )
            .await?;
        response.model = selected;
        Ok(response)
    }
}

#[cfg(test)]
#[path = "model_roles_tests.rs"]
mod tests;
