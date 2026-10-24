//! Prompt-cache accounting. Every main-loop prefix break is either declared by
//! the harness with a reason, or reported as undeclared; every cache miss below
//! the client-side expected cacheable prefix is reported as unexplained, per
//! provider and model. This is diagnostic state only, never history.
use crate::{ModelRequest, Usage, observation::fingerprint};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

impl crate::Store {
    /// The one prompt-cache ledger shared by every request of the host that owns
    /// this store, traced or not.
    pub fn cache_accounting(&self) -> Arc<CacheAccounting> {
        self.runtime().cache.clone()
    }
}

/// OpenCode Go reuses cached input in 64-token blocks.
pub const DEFAULT_CACHE_BLOCK_TOKENS: u64 = 64;
/// Earlier requests kept per provider/model for the longest-common-prefix search.
const PRIORS_PER_GROUP: usize = 16;
/// Request fields that select generation but are not part of the cached prompt.
const GENERATION_FIELDS: &[&str] = &[
    "max_tokens",
    "max_completion_tokens",
    "max_output_tokens",
    "stream",
    "stream_options",
];

/// The final provider request seen as a chain of message boundaries.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WireShape {
    /// Route provider id, for example `opencode-go`; empty when unknown.
    pub provider: String,
    /// Model as forwarded on the wire.
    pub model: String,
    /// `boundaries[0]` hashes every non-message prompt field; each later entry
    /// chains the previous hash with the next wire message (SHA-256, hex).
    pub boundaries: Vec<String>,
    /// Cumulative serialized bytes at each boundary.
    pub boundary_bytes: Vec<u64>,
    /// Bytes of the complete serialized wire body.
    pub wire_bytes: u64,
    pub cache_block_tokens: u64,
}

impl WireShape {
    /// Shape a JSON wire body whose ordered conversation is the array `items`
    /// (`messages` for Chat and Messages, `input` for Responses).
    pub fn from_body(body: &Value, items: &str, cache_block_tokens: u64) -> Self {
        let mut head = without_cache_markers(body);
        let messages = head
            .as_object_mut()
            .and_then(|map| {
                for field in GENERATION_FIELDS {
                    map.remove(*field);
                }
                map.remove(items)
            })
            .and_then(|value| match value {
                Value::Array(values) => Some(values),
                _ => None,
            })
            .unwrap_or_default();
        let head = head.to_string();
        let mut hash = fingerprint(head.as_bytes());
        let mut bytes = head.len() as u64;
        let mut shape = Self {
            boundaries: vec![hash.clone()],
            boundary_bytes: vec![bytes],
            wire_bytes: body.to_string().len() as u64,
            cache_block_tokens: cache_block_tokens.max(1),
            ..Self::default()
        };
        for message in &messages {
            let raw = message.to_string();
            bytes += raw.len() as u64;
            hash = fingerprint(format!("{hash}{raw}").as_bytes());
            shape.boundaries.push(hash.clone());
            shape.boundary_bytes.push(bytes);
        }
        shape
    }
    fn group(&self) -> String {
        if self.provider.is_empty() {
            self.model.clone()
        } else {
            format!("{}/{}", self.provider, self.model)
        }
    }
}

/// Cache markers steer where the provider caches; they are not prompt content,
/// so a moved breakpoint is not a prefix break.
fn without_cache_markers(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| *key != "cache_control")
                .map(|(key, value)| (key.clone(), without_cache_markers(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(without_cache_markers).collect()),
        other => other.clone(),
    }
}

/// Which loop a request belongs to. Only the main loop must be append-only;
/// summary forks and decisions are measured against the shared cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    Main,
    SummaryFork,
    Decider,
}

/// Harness-known request components; a change in one declares the break.
pub fn request_components(request: &ModelRequest) -> Vec<(&'static str, String)> {
    let hash = |value: Value| fingerprint(value.to_string().as_bytes());
    vec![
        ("system_prompt_changed", hash(json!(request.system_prompt))),
        ("tool_declarations_changed", hash(json!(request.tools))),
        (
            "tool_admission_changed",
            hash(json!(request.disabled_tools)),
        ),
        (
            "reasoning_effort_changed",
            hash(json!(request.reasoning_effort)),
        ),
    ]
}

struct Pending {
    session: String,
    kind: RequestKind,
    shape: WireShape,
    components: Vec<(&'static str, String)>,
}
struct Prior {
    boundaries: Vec<String>,
    boundary_bytes: Vec<u64>,
    input_tokens: Option<u64>,
}
struct LastMain {
    group: String,
    boundaries: Vec<String>,
    components: Vec<(&'static str, String)>,
    global_epoch: u64,
}
#[derive(Default)]
struct State {
    pending: HashMap<String, Pending>,
    priors: HashMap<String, Vec<Prior>>,
    last_main: HashMap<String, LastMain>,
    declared: HashMap<String, String>,
    global: Option<(u64, String)>,
}

/// One finished request's accounting.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheOutcome {
    pub provider: String,
    pub model: String,
    pub kind: Option<RequestKind>,
    pub input_tokens: Option<u64>,
    /// Longest common boundary prefix with an earlier request of the same
    /// provider/model, rounded down to the cache block. None when unmeasurable.
    pub expected_cacheable_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub unexplained_miss_tokens: Option<u64>,
    /// Main loop only: the request did not extend the previous main request.
    pub prefix_break: Option<PrefixBreak>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrefixBreak {
    pub declared: bool,
    /// The harness reason, or `undeclared`.
    pub reason: String,
    pub expected_uncached_tokens: Option<u64>,
}

impl CacheOutcome {
    pub fn metrics(&self) -> Value {
        json!({"cache_group":{"provider":self.provider,"model":self.model,"kind":self.kind},"expected_cacheable_tokens":self.expected_cacheable_tokens,"measured_cache_read_tokens":self.cache_read_tokens,"unexplained_miss_tokens":self.unexplained_miss_tokens,"prefix_break":self.prefix_break.as_ref().map(|b| &b.reason)})
    }
    /// The explicit `cache_bust` record, present only for a main-loop break.
    pub fn bust_event(&self) -> Option<Value> {
        let bust = self.prefix_break.as_ref()?;
        Some(
            json!({"provider":self.provider,"model":self.model,"declared":bust.declared,"reason":bust.reason,"expected_cacheable_tokens":self.expected_cacheable_tokens,"expected_uncached_tokens":bust.expected_uncached_tokens}),
        )
    }
}

/// Shared by every request that can hit the same provider cache (main loop,
/// summary forks and decisions in one process).
#[derive(Default)]
pub struct CacheAccounting {
    state: Mutex<State>,
    /// Provider-independent tier diagnostics, including unshaped requests.
    pub context: Arc<crate::ContextState>,
}

impl CacheAccounting {
    /// Declare that the harness changed the next main request's prefix.
    /// `None` applies to every session (for example a configuration reload).
    pub fn declare(&self, session: Option<&str>, reason: &str) {
        self.context.declare(session, reason);
        let mut state = self.state.lock().unwrap();
        match session {
            Some(session) => {
                state.declared.insert(session.to_owned(), reason.to_owned());
            }
            None => {
                let epoch = state.global.as_ref().map_or(1, |(epoch, _)| epoch + 1);
                state.global = Some((epoch, reason.to_owned()));
            }
        }
    }
    /// Record a request about to be sent; returns `model_started` metrics.
    pub fn start(
        &self,
        request_id: &str,
        session: &str,
        kind: RequestKind,
        shape: WireShape,
        components: Vec<(&'static str, String)>,
    ) -> Value {
        let metrics = json!({"cache_group":{"provider":shape.provider,"model":shape.model,"kind":kind},"wire_bytes":shape.wire_bytes,"wire_boundaries":shape.boundaries.len(),"wire_head_sha256":shape.boundaries.first(),"wire_tail_sha256":shape.boundaries.last()});
        self.state.lock().unwrap().pending.insert(
            request_id.to_owned(),
            Pending {
                session: session.to_owned(),
                kind,
                shape,
                components,
            },
        );
        metrics
    }
    /// Drop a request that failed before producing usage.
    pub fn abandon(&self, request_id: &str) {
        self.state.lock().unwrap().pending.remove(request_id);
    }
    /// Account a finished request. `selected_route` is the route that actually
    /// answered (`provider/model`) when the host fell back to another one.
    /// `single_round` is false when usage sums several physical requests.
    pub fn finish(
        &self,
        request_id: &str,
        usage: Option<&Usage>,
        selected_route: Option<&str>,
        single_round: bool,
    ) -> Option<CacheOutcome> {
        let mut state = self.state.lock().unwrap();
        let Pending {
            session,
            kind,
            mut shape,
            components,
        } = state.pending.remove(request_id)?;
        let mut route_switched = false;
        if let Some((provider, model)) = selected_route.and_then(|r| r.split_once('/'))
            && (provider != shape.provider || model != shape.model)
        {
            // The boundaries describe the planned route's body, not the fallback's.
            route_switched = true;
            shape.provider = provider.to_owned();
            shape.model = model.to_owned();
        }
        let group = shape.group();
        let input_tokens = usage.filter(|_| single_round).map(|u| u.input_tokens);
        let expected = if route_switched {
            None
        } else {
            input_tokens.and_then(|input| {
                let common = state
                    .priors
                    .get(&group)
                    .into_iter()
                    .flatten()
                    .map(|prior| common_tokens(prior, &shape.boundaries))
                    .try_fold(0, |best, tokens| tokens.map(|t| best.max(t)))?;
                let block = shape.cache_block_tokens.max(1);
                Some((common / block * block).min(input))
            })
        };
        let cache_read = usage.filter(|_| single_round).map(|u| u.cache_read_tokens);
        let prefix_break = (kind == RequestKind::Main)
            .then(|| {
                let previous = state.last_main.get(&session)?;
                let reason = if route_switched || previous.group != group {
                    Some("route_or_model_switch".to_owned())
                } else if shape.boundaries.starts_with(&previous.boundaries) {
                    return None;
                } else {
                    state
                        .declared
                        .get(&session)
                        .cloned()
                        .or_else(|| {
                            state
                                .global
                                .as_ref()
                                .filter(|(epoch, _)| *epoch > previous.global_epoch)
                                .map(|(_, reason)| reason.clone())
                        })
                        .or_else(|| {
                            components
                                .iter()
                                .zip(&previous.components)
                                .find(|(now, before)| now.1 != before.1)
                                .map(|(now, _)| now.0.to_owned())
                        })
                };
                Some(PrefixBreak {
                    declared: reason.is_some(),
                    reason: reason.unwrap_or_else(|| "undeclared".into()),
                    expected_uncached_tokens: input_tokens
                        .zip(expected)
                        .map(|(input, expected)| input.saturating_sub(expected)),
                })
            })
            .flatten();
        if kind == RequestKind::Main {
            state.declared.remove(&session);
            let global_epoch = state.global.as_ref().map_or(0, |(epoch, _)| *epoch);
            state.last_main.insert(
                session,
                LastMain {
                    group: group.clone(),
                    boundaries: shape.boundaries.clone(),
                    components,
                    global_epoch,
                },
            );
        }
        if !route_switched {
            let priors = state.priors.entry(group).or_default();
            if priors.len() == PRIORS_PER_GROUP {
                priors.remove(0);
            }
            priors.push(Prior {
                boundaries: shape.boundaries,
                boundary_bytes: shape.boundary_bytes,
                input_tokens,
            });
        }
        Some(CacheOutcome {
            provider: shape.provider,
            model: shape.model,
            kind: Some(kind),
            input_tokens,
            expected_cacheable_tokens: expected,
            cache_read_tokens: cache_read,
            unexplained_miss_tokens: expected
                .zip(cache_read)
                .map(|(expected, read)| expected.saturating_sub(read)),
            prefix_break,
        })
    }
}

/// Tokens of `prior` that `boundaries` repeats. A full repeat is exact (the
/// prior's measured input); a partial one is prorated by wire bytes.
fn common_tokens(prior: &Prior, boundaries: &[String]) -> Option<u64> {
    let shared = prior
        .boundaries
        .iter()
        .zip(boundaries)
        .take_while(|(a, b)| a == b)
        .count();
    if shared == 0 {
        return Some(0);
    }
    let input = prior.input_tokens?;
    if shared == prior.boundaries.len() {
        return Some(input);
    }
    let total = (*prior.boundary_bytes.last()?).max(1);
    Some(input * prior.boundary_bytes[shared - 1] / total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(messages: &[&str]) -> WireShape {
        let body = json!({"model":"m","tools":[],"messages":messages,"stream":true});
        WireShape {
            provider: "go".into(),
            model: "m".into(),
            ..WireShape::from_body(&body, "messages", 64)
        }
    }
    fn usage(input: u64, read: u64) -> Usage {
        Usage {
            input_tokens: input,
            cache_read_tokens: read,
            ..Usage::default()
        }
    }

    #[test]
    fn append_is_expected_cached_and_rewrite_is_undeclared() {
        let cache = CacheAccounting::default();
        let run = |id: &str, messages: &[&str], input, read| {
            cache.start(id, "s", RequestKind::Main, shape(messages), vec![]);
            cache
                .finish(id, Some(&usage(input, read)), None, true)
                .unwrap()
        };
        let first = run("1", &["a"], 1000, 0);
        assert_eq!(first.expected_cacheable_tokens, Some(0));
        assert!(first.prefix_break.is_none());
        let second = run("2", &["a", "b", "c"], 1300, 896);
        // The whole first request, rounded down to the 64-token block.
        assert_eq!(second.expected_cacheable_tokens, Some(960));
        assert_eq!(second.unexplained_miss_tokens, Some(64));
        assert!(second.prefix_break.is_none());
        let third = run("3", &["a", "x"], 1100, 0);
        let bust = third.prefix_break.unwrap();
        assert!(!bust.declared);
        assert_eq!(bust.reason, "undeclared");
        cache.declare(Some("s"), "lcm_apply");
        let fourth = run("4", &["y"], 500, 0);
        assert_eq!(fourth.prefix_break.unwrap().reason, "lcm_apply");
        // A generation-only field never breaks the prefix.
        let a = WireShape::from_body(&json!({"messages":["a"],"max_tokens":1}), "messages", 64);
        let b = WireShape::from_body(&json!({"messages":["a"],"max_tokens":9}), "messages", 64);
        assert_eq!(a.boundaries, b.boundaries);
    }
}
