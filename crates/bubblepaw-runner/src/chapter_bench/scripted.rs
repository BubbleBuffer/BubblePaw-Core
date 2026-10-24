//! Deterministic, credential-free providers for the chapter benchmark dry run.
use super::{BenchOptions, parse_arm};
use crate::provider::ProviderRegistry;
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use bubblepaw_core::{
    Answer, AnswerKind, CancellationToken, DecisionProvider, DecisionRequest, DecisionResponse,
    DecisionUsage, ModelRequest, ModelResponse, Provider, QuestionKind, TextCallback, Usage,
};
use serde_json::json;
use std::{collections::BTreeSet, sync::Arc};

pub fn registry(options: &BenchOptions) -> Result<Arc<ProviderRegistry>> {
    let provider = Arc::new(Offline);
    let mut registry = ProviderRegistry::new("opencode", provider.clone()).with_strict_routes();
    let mut llm_routes = BTreeSet::from(["opencode".to_owned()]);
    let mut decision_routes = BTreeSet::new();
    for value in &options.arms {
        let arm = parse_arm(value)?;
        let model = arm.model.strip_prefix("llm/");
        let route = model
            .unwrap_or(&arm.model)
            .split_once('/')
            .context("Scripted arm model must be qualified as provider/model")?
            .0;
        if model.is_some() {
            if llm_routes.insert(route.to_owned()) {
                registry.insert(route, provider.clone())?;
            }
        } else if decision_routes.insert(route.to_owned()) {
            registry.insert_decision(route, provider.clone())?;
        }
    }
    Ok(Arc::new(registry))
}

struct Offline;

fn check_cancelled(cancellation: &CancellationToken) -> Result<()> {
    ensure!(
        !cancellation.is_cancelled(),
        "Scripted chapter benchmark request cancelled"
    );
    Ok(())
}

fn usage(input: &str, output: &str) -> Usage {
    Usage {
        input_tokens: input.chars().count().div_ceil(4).max(1) as u64,
        output_tokens: output.chars().count().div_ceil(4).max(1) as u64,
        ..Usage::default()
    }
}

// The accumulated old chapter and the agent-before excerpt can contain a prior
// transition. Only the numbered candidate itself is eligible for this fixture.
fn boundary_choice(input: &str) -> String {
    let Some((_, candidates)) =
        input.rsplit_once("NEXT BOUNDARIES (typed request or labeled episode summary):\n")
    else {
        return "none".into();
    };
    let mut current = None;
    for line in candidates.lines() {
        let start = line
            .split_once(". (agent before: ")
            .and_then(|(number, rest)| {
                number.parse::<usize>().ok()?;
                let (_, candidate) = rest.split_once(") ")?;
                Some((number, candidate))
            });
        let candidate = match start {
            Some((number, candidate)) => {
                current = Some(number);
                candidate
            }
            None => line,
        };
        if candidate.contains("[boundary]")
            && let Some(number) = current
        {
            return number.to_owned();
        }
    }
    "none".into()
}

fn narrative(request: &ModelRequest) -> String {
    let current = request.messages.iter().rev().find_map(|message| {
        message
            .content
            .rsplit_once("CURRENT EPISODE:\n")
            .map(|(_, episode)| {
                episode
                    .split_once("\n\nEND OF CURRENT EPISODE.")
                    .map_or(episode, |(body, _)| body)
            })
    });
    let marker = if current.is_some_and(|episode| episode.contains("[boundary]")) {
        " The current episode records the fixture's subject transition [boundary]."
    } else {
        ""
    };
    format!(
        "# Scripted episode\n\nGoal and progress: The offline fixture retained the current episode's work state.{marker}\n\nEvidence: This is deterministic scripted output; no live model was called.\n\nOpen: Continue from the retained source conversation."
    )
}

#[async_trait]
impl Provider for Offline {
    async fn generate(
        &self,
        request: ModelRequest,
        _on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        check_cancelled(&cancellation)?;
        let mut response = ModelResponse {
            text: narrative(&request),
            ..ModelResponse::default()
        };
        let input = serde_json::to_string(&request)?;
        response.usage = Some(usage(&input, &serde_json::to_string(&response)?));
        Ok(response)
    }

    async fn generate_decision(
        &self,
        request: ModelRequest,
        _on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        check_cancelled(&cancellation)?;
        let choice = request
            .messages
            .iter()
            .rev()
            .find(|message| {
                message
                    .content
                    .contains("NEXT BOUNDARIES (typed request or labeled episode summary):\n")
            })
            .map_or_else(
                || "none".into(),
                |message| boundary_choice(&message.content),
            );
        let text = json!({"choice":choice}).to_string();
        let metering = usage(&serde_json::to_string(&request)?, &text);
        Ok(ModelResponse {
            text,
            usage: Some(metering),
            ..ModelResponse::default()
        })
    }
}

#[async_trait]
impl DecisionProvider for Offline {
    async fn decide(
        &self,
        request: DecisionRequest,
        cancellation: CancellationToken,
    ) -> Result<DecisionResponse> {
        check_cancelled(&cancellation)?;
        let selected = boundary_choice(&request.input);
        let answers = request
            .questions
            .iter()
            .map(|question| {
                let kind = match &question.kind {
                    QuestionKind::Choice { choices } => {
                        let choice = choices
                            .iter()
                            .find(|choice| choice.value == selected)
                            .or_else(|| choices.iter().find(|choice| choice.value == "none"))
                            .or_else(|| choices.first());
                        match choice {
                            Some(choice) => AnswerKind::Choice {
                                choice: choice.value.clone(),
                                probabilities: choices
                                    .iter()
                                    .map(|option| {
                                        (
                                            option.value.clone(),
                                            if option.value == choice.value {
                                                1.0
                                            } else {
                                                0.0
                                            },
                                        )
                                    })
                                    .collect(),
                            },
                            None => AnswerKind::Refusal,
                        }
                    }
                    _ => AnswerKind::Refusal,
                };
                Answer {
                    name: question.name.clone(),
                    kind,
                }
            })
            .collect::<Vec<_>>();
        let metering = usage(
            &serde_json::to_string(&request)?,
            &serde_json::to_string(&answers)?,
        );
        Ok(DecisionResponse {
            answers,
            model: request.model,
            usage: DecisionUsage {
                cost_usd: Some(0.0),
                ..DecisionUsage::from(metering)
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bubblepaw_core::{DecisionChoice, Message, Question, Role};

    #[test]
    fn boundaries_ignore_the_old_chapter_and_agent_excerpt() {
        let prefix = "ACCUMULATED OPEN CHAPTER STATE:\nOld [boundary]\n\nNEXT BOUNDARIES (typed request or labeled episode summary):\n";
        assert_eq!(
            boundary_choice(&format!(
                "{prefix}1. (agent before: old [boundary]) continuing work"
            )),
            "none"
        );
        assert_eq!(
            boundary_choice(&format!(
                "{prefix}1. (agent before: old [boundary]) continuing\n2. (agent before: done) current [boundary]"
            )),
            "2"
        );
        assert_eq!(
            boundary_choice(&format!(
                "{prefix}1. (agent before: old [boundary]) EPISODE SUMMARY (not typed user input):\n# First episode\nContinuing.\n2. (agent before: old [boundary]) EPISODE SUMMARY (not typed user input):\n# Second episode\nNew phase [boundary]"
            )),
            "2"
        );
    }

    fn message(content: &str) -> Message {
        Message {
            id: String::new(),
            session_id: String::new(),
            role: Role::User,
            origin: None,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            provider_state: None,
            is_error: false,
            created_at: String::new(),
        }
    }

    #[test]
    fn summary_preserves_only_current_episode_markers() {
        let request = |current| ModelRequest {
            messages: vec![message(&format!(
                "PREVIOUS EPISODE (read-only):\nold [boundary]\n\nCURRENT EPISODE:\n{current}\n\nEND OF CURRENT EPISODE. Summarize the episode above."
            ))],
            ..ModelRequest::default()
        };
        assert!(!narrative(&request("continuing")).contains("[boundary]"));
        assert!(narrative(&request("new phase [boundary]")).contains("[boundary]"));
    }

    #[tokio::test]
    async fn native_and_llm_decisions_agree_with_real_adapter_shape() -> Result<()> {
        let input = "ACCUMULATED OPEN CHAPTER STATE:\nold [boundary]\n\nNEXT BOUNDARIES (typed request or labeled episode summary):\n1. (agent before: old [boundary]) current [boundary]";
        let request = DecisionRequest {
            model: "fixture".into(),
            input: input.into(),
            questions: vec![Question {
                name: "choice".into(),
                instructions: "Choose a boundary".into(),
                kind: QuestionKind::Choice {
                    choices: ["none", "1"]
                        .into_iter()
                        .map(|value| DecisionChoice {
                            value: value.into(),
                            description: value.into(),
                        })
                        .collect(),
                },
            }],
            ..DecisionRequest::default()
        };
        let direct = Offline
            .decide(request.clone(), CancellationToken::new())
            .await?;
        let adapter = crate::provider::LlmDecisions::new(Arc::new(Offline));
        let chat = adapter.decide(request, CancellationToken::new()).await?;
        assert_eq!(direct.answers, chat.answers);
        assert!(direct.usage.input_tokens > 0 && chat.usage.output_tokens > 0);
        Ok(())
    }
}
