//! Decision adapters make exactly one provider call. Roles own sampling and retry.
use super::{cancelled, classify};
use anyhow::{Result, anyhow, ensure};
use async_trait::async_trait;
use bubblepaw_core::{
    Answer, AnswerKind, CancellationToken, DecisionProvider, DecisionRequest, DecisionResponse,
    DecisionUsage, Provider, ProviderErrorKind, QuestionKind,
};
use serde_json::{Map, Value, json};
use std::{sync::Arc, time::Duration};
#[path = "decisions_http.rs"]
mod http;
pub(super) use http::post_json;

pub(super) fn validate_url(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).map_err(|_| anyhow!("Invalid provider URL"))?;
    ensure!(
        matches!(parsed.scheme(), "http" | "https")
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.query().is_none()
            && parsed.fragment().is_none(),
        "Provider URL must be HTTP(S) without credentials/query/fragment"
    );
    Ok(())
}
pub(super) fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}
#[derive(Debug)]
struct DecisionParseError {
    message: String,
    usage: DecisionUsage,
}
impl std::fmt::Display for DecisionParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for DecisionParseError {}
pub fn decision_error_usage(error: &anyhow::Error) -> Option<&DecisionUsage> {
    error
        .downcast_ref::<DecisionParseError>()
        .map(|error| &error.usage)
}
pub(crate) fn with_decision_usage(error: anyhow::Error, usage: DecisionUsage) -> anyhow::Error {
    let mut source = bubblepaw_core::ProviderError::of(&error)
        .cloned()
        .unwrap_or_else(|| {
            bubblepaw_core::ProviderError::new(ProviderErrorKind::Fatal, error.to_string())
        });
    source.usage = Some(usage.clone().into());
    let metadata = DecisionParseError {
        message: error.to_string(),
        usage,
    };
    error.context(source).context(metadata)
}
fn metered_parse(
    result: Result<DecisionResponse>,
    usage: DecisionUsage,
) -> Result<DecisionResponse> {
    result.map_err(|error| metered_error(error, usage))
}
pub(super) fn metered_error(error: anyhow::Error, usage: DecisionUsage) -> anyhow::Error {
    let model_usage = usage.clone().into();
    metered_error_parts(error, usage, Some(model_usage))
}
pub(super) fn metered_model_error(
    error: anyhow::Error,
    usage: Option<bubblepaw_core::Usage>,
) -> anyhow::Error {
    let decision_usage = usage.as_ref().map(DecisionUsage::from).unwrap_or_default();
    metered_error_parts(error, decision_usage, usage)
}
fn metered_error_parts(
    error: anyhow::Error,
    usage: DecisionUsage,
    model_usage: Option<bubblepaw_core::Usage>,
) -> anyhow::Error {
    let mut source =
        bubblepaw_core::ProviderError::new(ProviderErrorKind::Fatal, error.to_string());
    source.usage = model_usage;
    let metadata = DecisionParseError {
        message: error.to_string(),
        usage,
    };
    error.context(source).context(metadata)
}
/// Full usage of a decision body. Chat's parser reads the provider aliases,
/// including DeepSeek's prompt-cache hit/miss counts. The decision parser's
/// top-level names keep the precedence they had over those aliases.
fn physical_usage(payload: &Value) -> bubblepaw_core::Usage {
    let value = &payload["usage"];
    let (mut result, _) = super::chat::response_usage(payload);
    if let Some(tokens) = value["input_tokens"].as_u64() {
        result.input_tokens = tokens;
    }
    if let Some(tokens) = value["output_tokens"].as_u64() {
        result.output_tokens = tokens;
    }
    let nested_cache_read = value["prompt_tokens_details"]["cached_tokens"]
        .as_u64()
        .or_else(|| value["input_tokens_details"]["cached_tokens"].as_u64());
    if nested_cache_read.is_none()
        && let Some(tokens) = value["cache_read_tokens"].as_u64()
    {
        result.cache_read_tokens = tokens;
    }
    let nested_cache_creation = value["prompt_tokens_details"]["cache_creation_tokens"]
        .as_u64()
        .or_else(|| value["input_tokens_details"]["cache_creation_tokens"].as_u64());
    if nested_cache_creation.is_none()
        && let Some(tokens) = value["cache_creation_tokens"].as_u64()
    {
        result.cache_creation_tokens = tokens;
    }
    result
}
/// What a decision response reports to its consumers: the full usage plus the
/// cost the endpoint reports, which `Usage` has no field for.
fn decision_usage(payload: &Value) -> DecisionUsage {
    let value = &payload["usage"];
    DecisionUsage {
        cost_usd: value["cost_usd"]
            .as_f64()
            .or_else(|| value["cost"].as_f64()),
        ..DecisionUsage::from(physical_usage(payload))
    }
}

fn distribution(value: &Value, labels: &[String]) -> Result<Vec<(String, f64)>> {
    ensure!(
        value
            .as_array()
            .is_none_or(|items| items.len() == labels.len()),
        "Decision probabilities do not match options"
    );
    let from_array = value.as_array().map(|items| {
        items
            .iter()
            .map(|item| {
                let label = item["label"]
                    .as_str()
                    .or_else(|| item["value"].as_str())
                    .ok_or_else(|| anyhow!("Invalid decision probability label"))?;
                Ok((label.to_owned(), item["probability"].clone()))
            })
            .collect::<Result<Map<String, Value>>>()
    });
    let object = match &from_array {
        Some(result) => result
            .as_ref()
            .map_err(|error| anyhow!(error.to_string()))?,
        None => value
            .as_object()
            .ok_or_else(|| anyhow!("Missing decision probabilities"))?,
    };
    ensure!(
        object.len() == labels.len(),
        "Decision probabilities do not match options"
    );
    let mut values = Vec::new();
    for label in labels {
        let p = object
            .get(label)
            .and_then(Value::as_f64)
            .filter(|p| p.is_finite() && (0.0..=1.0).contains(p))
            .ok_or_else(|| anyhow!("Invalid decision probability"))?;
        values.push((label.clone(), p));
    }
    let sum: f64 = values.iter().map(|(_, p)| p).sum();
    ensure!(
        (sum - 1.0).abs() <= 1e-3,
        "Decision probabilities do not sum to one"
    );
    for (_, p) in &mut values {
        *p /= sum;
    }
    Ok(values)
}
pub struct OpenRouterDecisions {
    client: reqwest::Client,
    key: String,
    endpoint: String,
}
/// OpenCode's System One transport shares Jev validation with OpenRouter, but
/// rejects a body `session_id`; session attribution is an HTTP header instead.
pub struct OpenCodeDecisions(OpenRouterDecisions);
impl OpenCodeDecisions {
    pub fn new(key: impl Into<String>, endpoint: impl Into<String>) -> Result<Self> {
        Ok(Self(OpenRouterDecisions::new(key, endpoint)?))
    }
    pub fn body(request: &DecisionRequest) -> Value {
        let mut body = OpenRouterDecisions::body(request);
        body.as_object_mut().unwrap().remove("session_id");
        body
    }
}
#[async_trait]
impl DecisionProvider for OpenCodeDecisions {
    async fn decide(
        &self,
        request: DecisionRequest,
        cancellation: CancellationToken,
    ) -> Result<DecisionResponse> {
        let mut meter = super::PhysicalRequest::new("opencode", &request.model);
        let mut reported_usage = None;
        let result: Result<DecisionResponse> = async {
            let payload = post_json(
                &self.0.client,
                &self.0.endpoint,
                &self.0.key,
                &Self::body(&request),
                "x-opencode-session",
                request
                    .session_id
                    .as_deref()
                    .filter(|id| !id.is_empty())
                    .unwrap_or("bubblepaw-decisions"),
                cancellation,
            )
            .await?;
            reported_usage = payload
                .get("usage")
                .is_some_and(Value::is_object)
                .then(|| physical_usage(&payload));
            let mut response = metered_parse(
                OpenRouterDecisions::parse(&request, &payload),
                decision_usage(&payload),
            )?;
            response.model = payload["model"].as_str().unwrap().into();
            Ok(response)
        }
        .await;
        let usage = reported_usage.or_else(|| match &result {
            Ok(response) => Some(bubblepaw_core::Usage::from(response.usage.clone())),
            Err(error) => {
                bubblepaw_core::ProviderError::of(error).and_then(|error| error.usage.clone())
            }
        });
        meter.finish(usage.as_ref(), result.is_err());
        result
    }
}
impl OpenRouterDecisions {
    pub fn new(key: impl Into<String>, endpoint: impl Into<String>) -> Result<Self> {
        let key = key.into();
        ensure!(!key.is_empty(), "API key is empty");
        let endpoint = endpoint.into();
        validate_url(&endpoint)?;
        Ok(Self {
            client: http_client()?,
            key,
            endpoint,
        })
    }
    pub fn body(request: &DecisionRequest) -> Value {
        let questions:Map<String,Value>=request.questions.iter().map(|question|{
            let criteria:Map<String,Value>=match &question.kind {
                QuestionKind::Predicate=>[("yes".into(),json!("Yes.")),("no".into(),json!("No."))].into_iter().collect(),
                QuestionKind::Choice{choices}=>choices.iter().map(|choice|(choice.value.clone(),json!(choice.description))).collect(),
                QuestionKind::Score{levels}=>levels.iter().map(|level|(level.label.clone(),json!(level.description))).collect(),
            };
            (question.name.clone(),json!({"type":"choice","instructions":question.instructions,"criteria":criteria}))
        }).collect();
        let mut body = json!({"model":request.model,"state":request.input,"questions":questions});
        if let Some(session) = &request.session_id {
            body["session_id"] = json!(session);
        }
        body
    }
    fn parse(request: &DecisionRequest, payload: &Value) -> Result<DecisionResponse> {
        let model = payload["model"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing decision model"))?;
        ensure!(
            model == request.model || model.starts_with(&format!("{}-", request.model)),
            "Unexpected Jev response model"
        );
        let answers = request
            .questions
            .iter()
            .map(|question| {
                let answer = &payload["answers"][&question.name];
                if answer["type"] == "refusal" {
                    return Ok(Answer {
                        name: question.name.clone(),
                        kind: AnswerKind::Refusal,
                    });
                }
                ensure!(answer["type"] == "choice", "Invalid Jev choice");
                let labels: Vec<String> = match &question.kind {
                    QuestionKind::Predicate => vec!["yes".into(), "no".into()],
                    QuestionKind::Choice { choices } => {
                        choices.iter().map(|choice| choice.value.clone()).collect()
                    }
                    QuestionKind::Score { levels } => {
                        levels.iter().map(|level| level.label.clone()).collect()
                    }
                };
                let choice = answer["choice"]
                    .as_str()
                    .filter(|choice| labels.iter().any(|label| label == choice))
                    .ok_or_else(|| anyhow!("Invalid Jev choice"))?
                    .to_owned();
                let probabilities = distribution(&answer["probabilities"], &labels)?;
                let kind = match question.kind {
                    QuestionKind::Predicate => AnswerKind::Predicate {
                        probability: probabilities
                            .iter()
                            .find(|(label, _)| label == "yes")
                            .unwrap()
                            .1,
                    },
                    QuestionKind::Choice { .. } => AnswerKind::Choice {
                        choice,
                        probabilities,
                    },
                    QuestionKind::Score { .. } => AnswerKind::Score {
                        score: probabilities
                            .iter()
                            .enumerate()
                            .map(|(index, (_, p))| index as f64 * p)
                            .sum(),
                        probabilities,
                    },
                };
                Ok(Answer {
                    name: question.name.clone(),
                    kind,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(DecisionResponse {
            answers,
            model: request.model.clone(),
            usage: decision_usage(payload),
        })
    }
}
#[async_trait]
impl DecisionProvider for OpenRouterDecisions {
    async fn decide(
        &self,
        request: DecisionRequest,
        cancellation: CancellationToken,
    ) -> Result<DecisionResponse> {
        let mut meter = super::PhysicalRequest::new("openrouter", &request.model);
        let mut reported_usage = None;
        let result: Result<DecisionResponse> = async {
            let payload = post_json(
                &self.client,
                &self.endpoint,
                &self.key,
                &Self::body(&request),
                "x-session-id",
                request.session_id.as_deref().unwrap_or(""),
                cancellation,
            )
            .await?;
            reported_usage = payload
                .get("usage")
                .is_some_and(Value::is_object)
                .then(|| physical_usage(&payload));
            metered_parse(Self::parse(&request, &payload), decision_usage(&payload))
        }
        .await;
        let usage = reported_usage.or_else(|| match &result {
            Ok(response) => Some(bubblepaw_core::Usage::from(response.usage.clone())),
            Err(error) => {
                bubblepaw_core::ProviderError::of(error).and_then(|error| error.usage.clone())
            }
        });
        meter.finish(usage.as_ref(), result.is_err());
        result
    }
}
pub struct OpenAiDecisions {
    client: reqwest::Client,
    key: String,
    endpoint: String,
}
impl OpenAiDecisions {
    /// `base` is an origin, or an API base ending in `/v1`.
    pub fn new(key: impl Into<String>, base: impl Into<String>) -> Result<Self> {
        let key = key.into();
        ensure!(!key.is_empty(), "API key is empty");
        let base = base.into();
        validate_url(&base)?;
        let base = base.trim_end_matches('/');
        let endpoint = if base.ends_with("/v1") {
            format!("{base}/decisions")
        } else {
            format!("{base}/v1/decisions")
        };
        Ok(Self {
            client: http_client()?,
            key,
            endpoint,
        })
    }
    pub fn body(request: &DecisionRequest) -> Value {
        let questions: Vec<Value> = request
            .questions
            .iter()
            .map(|question| {
                let mut value = json!({"name":question.name,"instructions":question.instructions});
                match &question.kind {
                    QuestionKind::Predicate => value["type"] = json!("predicate"),
                    QuestionKind::Choice { choices } => {
                        value["type"] = json!("choice");
                        value["choices"] = json!(choices);
                    }
                    QuestionKind::Score { levels } => {
                        value["type"] = json!("score");
                        value["levels"] = json!(levels);
                    }
                }
                value
            })
            .collect();
        json!({"model":request.model,"input":request.input,"questions":questions})
    }
    fn parse(request: &DecisionRequest, payload: &Value) -> Result<DecisionResponse> {
        let raw = payload["answers"]
            .as_array()
            .ok_or_else(|| anyhow!("Missing decision answers"))?;
        ensure!(
            raw.len() == request.questions.len(),
            "Decision answers do not match questions"
        );
        let answers = request
            .questions
            .iter()
            .map(|question| {
                let answer = raw
                    .iter()
                    .find(|answer| answer["name"] == question.name)
                    .ok_or_else(|| anyhow!("Missing decision answer"))?;
                let kind = if answer["type"] == "refusal"
                    || answer
                        .get("refusal")
                        .is_some_and(|refusal| !refusal.is_null() && refusal != false)
                {
                    AnswerKind::Refusal
                } else {
                    match &question.kind {
                        QuestionKind::Predicate => {
                            ensure!(answer["type"] == "predicate", "Invalid predicate answer");
                            let probability = answer["probability"]
                                .as_f64()
                                .filter(|p| p.is_finite() && (0.0..=1.0).contains(p))
                                .ok_or_else(|| anyhow!("Invalid predicate probability"))?;
                            AnswerKind::Predicate { probability }
                        }
                        QuestionKind::Choice { choices } => {
                            ensure!(answer["type"] == "choice", "Invalid choice answer");
                            let labels = choices
                                .iter()
                                .map(|choice| choice.value.clone())
                                .collect::<Vec<_>>();
                            let choice = answer["choice"]
                                .as_str()
                                .filter(|value| labels.iter().any(|label| label == value))
                                .ok_or_else(|| anyhow!("Invalid decision choice"))?
                                .into();
                            AnswerKind::Choice {
                                choice,
                                probabilities: distribution(&answer["probabilities"], &labels)?,
                            }
                        }
                        QuestionKind::Score { levels } => {
                            ensure!(answer["type"] == "score", "Invalid score answer");
                            let labels = levels
                                .iter()
                                .map(|level| level.label.clone())
                                .collect::<Vec<_>>();
                            let score = answer["score"]
                                .as_f64()
                                .filter(|value| {
                                    value.is_finite()
                                        && (0.0..=levels.len().saturating_sub(1) as f64)
                                            .contains(value)
                                })
                                .ok_or_else(|| anyhow!("Invalid decision score"))?;
                            AnswerKind::Score {
                                score,
                                probabilities: distribution(&answer["probabilities"], &labels)?,
                            }
                        }
                    }
                };
                Ok(Answer {
                    name: question.name.clone(),
                    kind,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(DecisionResponse {
            answers,
            model: payload["model"].as_str().unwrap_or(&request.model).into(),
            usage: decision_usage(payload),
        })
    }
}
#[async_trait]
impl DecisionProvider for OpenAiDecisions {
    async fn decide(
        &self,
        request: DecisionRequest,
        cancellation: CancellationToken,
    ) -> Result<DecisionResponse> {
        let mut meter = super::PhysicalRequest::new("openai", &request.model);
        let mut reported_usage = None;
        let result: Result<DecisionResponse> = async {
            let payload = post_json(
                &self.client,
                &self.endpoint,
                &self.key,
                &Self::body(&request),
                "x-session-id",
                request.session_id.as_deref().unwrap_or(""),
                cancellation,
            )
            .await?;
            reported_usage = payload
                .get("usage")
                .is_some_and(Value::is_object)
                .then(|| physical_usage(&payload));
            metered_parse(Self::parse(&request, &payload), decision_usage(&payload))
        }
        .await;
        let usage = reported_usage.or_else(|| match &result {
            Ok(response) => Some(bubblepaw_core::Usage::from(response.usage.clone())),
            Err(error) => {
                bubblepaw_core::ProviderError::of(error).and_then(|error| error.usage.clone())
            }
        });
        meter.finish(usage.as_ref(), result.is_err());
        result
    }
}
#[path = "decision_request.rs"]
mod request_assembly;

pub struct LlmDecisions {
    provider: Arc<dyn Provider>,
    context_route: crate::context_routes::ContextRoute,
}
impl LlmDecisions {
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self {
            provider,
            context_route: Default::default(),
        }
    }
    pub fn prompt(request: &DecisionRequest) -> Result<String> {
        if let [question] = request.questions.as_slice()
            && question.name == "choice"
            && let QuestionKind::Choice { choices } = &question.kind
        {
            return Ok(format!(
                "{}\nReturn only JSON {{\"choice\":\"<option>\"}}.\nAllowed options: {}",
                question.instructions,
                serde_json::to_string(
                    &choices
                        .iter()
                        .map(|choice| &choice.value)
                        .collect::<Vec<_>>()
                )?
            ));
        }
        let mut prompt = String::from(
            "Answer each question using the supplied input. Return only a JSON object {name: value}, with exactly one entry per question. Predicates use true or false; choices use a choice value; scores use a level label. Use low reasoning.\n",
        );
        for question in &request.questions {
            let choices = match &question.kind {
                QuestionKind::Predicate => json!([true, false]),
                QuestionKind::Choice { choices } => json!(choices),
                QuestionKind::Score { levels } => json!(levels),
            };
            prompt.push_str(&format!(
                "{}: {}\nAllowed answers: {}\n",
                question.name, question.instructions, choices
            ));
        }
        Ok(prompt)
    }
    fn answers(request: &DecisionRequest, text: &str) -> Vec<Answer> {
        let object = serde_json::from_str::<Value>(text).ok().filter(|value| {
            value
                .as_object()
                .is_some_and(|object| object.len() == request.questions.len())
        });
        request
            .questions
            .iter()
            .map(|question| {
                let value = object
                    .as_ref()
                    .and_then(|object| object.get(&question.name));
                let kind = match &question.kind {
                    QuestionKind::Predicate => {
                        value
                            .and_then(Value::as_bool)
                            .map(|value| AnswerKind::Predicate {
                                probability: if value { 1.0 } else { 0.0 },
                            })
                    }
                    QuestionKind::Choice { choices } => value
                        .and_then(Value::as_str)
                        .filter(|value| choices.iter().any(|choice| choice.value == *value))
                        .map(|value| AnswerKind::Choice {
                            choice: value.into(),
                            probabilities: choices
                                .iter()
                                .map(|choice| {
                                    (
                                        choice.value.clone(),
                                        if choice.value == value { 1.0 } else { 0.0 },
                                    )
                                })
                                .collect(),
                        }),
                    QuestionKind::Score { levels } => value
                        .and_then(Value::as_str)
                        .and_then(|value| {
                            levels
                                .iter()
                                .position(|level| level.label == value)
                                .map(|index| (value, index))
                        })
                        .map(|(value, index)| AnswerKind::Score {
                            score: index as f64,
                            probabilities: levels
                                .iter()
                                .map(|level| {
                                    (
                                        level.label.clone(),
                                        if level.label == value { 1.0 } else { 0.0 },
                                    )
                                })
                                .collect(),
                        }),
                }
                .unwrap_or(AnswerKind::Refusal);
                Answer {
                    name: question.name.clone(),
                    kind,
                }
            })
            .collect()
    }
    pub async fn decide_with_reasoning(
        &self,
        request: DecisionRequest,
        cancellation: CancellationToken,
    ) -> Result<(DecisionResponse, Option<String>)> {
        Self::decide_using(
            self.provider.as_ref(),
            request,
            cancellation,
            None,
            &self.context_route,
        )
        .await
    }
    pub(crate) async fn decide_using(
        provider: &dyn Provider,
        request: DecisionRequest,
        cancellation: CancellationToken,
        reported_model: Option<&str>,
        context_route: &crate::context_routes::ContextRoute,
    ) -> Result<(DecisionResponse, Option<String>)> {
        let session = request.session_id.as_deref().unwrap_or_default();
        let kind = Self::context_kind(&request);
        let route = context_route.inherited();
        let model_request = route.build_side(Self::model_request(&request)?, session, &kind, None);
        let response = provider
            .generate_decision(model_request, Arc::new(|_| {}), cancellation)
            .await?;
        route.finish_response(session, &kind, &request.model, &response);
        if !response.tool_calls.is_empty() {
            return Err(metered_error(
                anyhow!("Decision model returned a tool call with tools disabled"),
                response
                    .usage
                    .as_ref()
                    .map(DecisionUsage::from)
                    .unwrap_or_default(),
            ));
        }
        let reasoning = response
            .provider_state
            .as_ref()
            .and_then(super::ProviderRegistry::reasoning);
        Ok((
            DecisionResponse {
                answers: Self::answers(&request, &response.text),
                model: super::ProviderRegistry::selected_model(&response)
                    .and_then(|route| route.split_once('/').map(|(_, model)| model))
                    .unwrap_or_else(|| reported_model.unwrap_or(&request.model))
                    .to_owned(),
                usage: response.usage.map(DecisionUsage::from).unwrap_or_default(),
            },
            reasoning,
        ))
    }
}
#[async_trait]
impl DecisionProvider for LlmDecisions {
    async fn decide(
        &self,
        request: DecisionRequest,
        cancellation: CancellationToken,
    ) -> Result<DecisionResponse> {
        self.decide_with_reasoning(request, cancellation)
            .await
            .map(|(response, _)| response)
    }
}
