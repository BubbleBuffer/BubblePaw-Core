//! Provider-independent context state. The ordinary build classifies an unchanged
//! request; build_persisted validates and commits queued reminders as a new tail
//! before invoking that same classifier once. Prior history is never restored.
use crate::{ModelRequest, observation::fingerprint};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Mutex,
};
#[path = "cold_notes.rs"]
mod cold_notes;
#[path = "context_fork.rs"]
mod fork;
#[path = "context_log.rs"]
mod log;
#[path = "reminders.rs"]
mod reminders;
#[path = "context_writer.rs"]
mod writer;
pub use log::RequestLogRow;
pub use reminders::{
    LegacyReminderSource, Reminder, ReminderDelivery, ReminderMetadata, ReminderRecord,
};
pub use writer::ContextWriter;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierFingerprint {
    pub sha256: String,
    pub bytes: usize,
    pub message_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContextClassification {
    First,
    Append,
    FrozenChange,
    ColdChange,
    /// Index within the history tier, excluding explicitly marked cold messages.
    HistoryRewrite {
        first_differing_index: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBuild {
    pub request_kind: String,
    pub frozen: TierFingerprint,
    pub cold: TierFingerprint,
    pub history: TierFingerprint,
    pub classification: ContextClassification,
    pub declared: bool,
    pub declaration_reason: Option<String>,
    /// Invalid host annotations are reported without rejecting a model request.
    pub invalid_cold_indexes: usize,
    /// Set when the host's preparation hook made this frozen_change; such a
    /// change is never declared, so it is the warning case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frozen_writer: Option<String>,
    /// This request's position in its lineage, from 1 in each process.
    #[serde(default)]
    pub seq: u64,
    /// Since the request whose cache this one reads (a fork's parent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_previous_ms: Option<u64>,
}
impl ContextBuild {
    pub fn metrics(&self) -> Value {
        serde_json::to_value(self).expect("context metadata is JSON serializable")
    }
}

/// The original request, with the diagnostics computed at its final assembly.
pub struct BuiltContext {
    pub request: ModelRequest,
    pub record: ContextBuild,
    pub tiers: CacheTiers,
}

/// Tier ends of a built request as message counts, for provider cache
/// placement. `cold_end` is one past the last cold message. `previous_len` is
/// the length of this lineage's previous request when this one appends to it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheTiers {
    pub cold_end: Option<usize>,
    pub previous_len: Option<usize>,
}

struct Previous {
    frozen: TierFingerprint,
    cold: TierFingerprint,
    history: Vec<String>,
    declaration_epoch: u64,
    /// Route and generation settings, so a fresh side call can say why it is not a fork.
    model: String,
    settings: String,
    /// The cold split this request was measured with; a fork reuses it.
    cold_indexes: Vec<usize>,
    /// Frozen tier before and after the host hook, when the harness noted it.
    preparation: Option<(String, String)>,
}
#[derive(Default)]
struct State {
    previous: HashMap<(String, String), Previous>,
    epoch: u64,
    global: Option<(u64, String)>,
    sessions: HashMap<String, (u64, String)>,
    tool_generations: HashMap<String, u64>,
    staged_notes: HashMap<String, Vec<cold_notes::ColdNote>>,
    served_notes: HashMap<(String, String), Vec<cold_notes::ColdNote>>,
    preparations: HashMap<(String, String), (String, String)>,
    log: log::Log,
}
impl State {
    /// The newest declaration that applies to the session.
    fn declaration(&self, session: &str) -> Option<(u64, String)> {
        self.sessions
            .get(session)
            .into_iter()
            .chain(self.global.iter())
            .max_by_key(|(epoch, _)| epoch)
            .cloned()
    }
}

/// One prior snapshot per session/request-kind lineage, shared by the host's
/// existing CacheAccounting. It also works for providers without wire shapes.
#[derive(Default)]
pub struct ContextState {
    state: Mutex<State>,
    reminders: Mutex<HashMap<(String, String), reminders::ReminderQueue>>,
}
impl ContextState {
    /// Called by CacheAccounting::declare, independently of legacy declaration
    /// consumption. Every lineage observes a declaration once at its next build.
    pub fn declare(&self, session: Option<&str>, reason: &str) {
        let mut state = self.state.lock().unwrap();
        state.epoch += 1;
        let declaration = (state.epoch, reason.to_owned());
        match session {
            Some(session) => {
                state.sessions.insert(session.to_owned(), declaration);
            }
            None => state.global = Some(declaration),
        }
    }

    /// Record the tool generation a run took for this session. True when an earlier
    /// run of the session saw a different generation, so the republish is declared.
    pub fn observe_tool_generation(&self, session: &str, generation: u64) -> bool {
        let mut state = self.state.lock().unwrap();
        state
            .tool_generations
            .insert(session.to_owned(), generation)
            .is_some_and(|previous| previous != generation)
    }

    /// Hash the assembled request without changing any content or ordering;
    /// build_persisted places the cold notes first.
    /// Frozen is exactly system prompt, tool declarations and disabled admission.
    /// Moving the declarations into `disabled_tools` is therefore a frozen change;
    /// an unavailable round that keeps them is not. Generation settings,
    /// route/model and output schema are outside this tier.
    /// Cold membership is host metadata, never inferred from message text.
    pub fn build(
        &self,
        request: ModelRequest,
        session: &str,
        request_kind: &str,
        cold_message_indexes: &[usize],
    ) -> BuiltContext {
        let indexes: BTreeSet<_> = cold_message_indexes.iter().copied().collect();
        let measured = measure(&request, &indexes, request.messages.len());
        let Measured {
            frozen,
            cold,
            history,
            hashes: history_hashes,
            ..
        } = measured.clone();
        let mut state = self.state.lock().unwrap();
        let declaration = state.declaration(session);
        let key = (session.to_owned(), request_kind.to_owned());
        let preparation = state.preparations.remove(&key);
        let previous = state.previous.get(&key);
        let previous_len = previous.map(|before| before.cold.message_count + before.history.len());
        let previous_history = previous.map_or(0, |before| before.history.len());
        let classification = match previous {
            None => ContextClassification::First,
            Some(before) if before.frozen != frozen => ContextClassification::FrozenChange,
            Some(before) if before.cold != cold => ContextClassification::ColdChange,
            Some(before) if history_hashes.starts_with(&before.history) => {
                ContextClassification::Append
            }
            Some(before) => ContextClassification::HistoryRewrite {
                first_differing_index: before
                    .history
                    .iter()
                    .zip(&history_hashes)
                    .take_while(|(a, b)| a == b)
                    .count(),
            },
        };
        let changed = !matches!(
            classification,
            ContextClassification::First | ContextClassification::Append
        );
        let frozen_writer = (classification == ContextClassification::FrozenChange
            && writer::hook_changed_frozen(previous, preparation.as_ref()))
        .then(|| "context_preparation".to_owned());
        let reason = declaration
            .as_ref()
            .filter(|(epoch, _)| {
                changed
                    && frozen_writer.is_none()
                    && previous.is_some_and(|prior| *epoch > prior.declaration_epoch)
            })
            .map(|(_, reason)| reason.clone());
        if frozen_writer.is_some() {
            tracing::warn!(
                "context preparation changed the system prompt or tools of session {session}; undeclared frozen_change"
            );
        }
        state.previous.insert(
            key,
            Previous {
                frozen: frozen.clone(),
                cold: cold.clone(),
                history: history_hashes,
                declaration_epoch: declaration.map_or(0, |(epoch, _)| epoch),
                model: request.model.clone(),
                settings: settings(&request),
                cold_indexes: indexes.range(..request.messages.len()).copied().collect(),
                preparation,
            },
        );
        let invalid_cold_indexes = indexes.range(request.messages.len()..).count();
        let tiers = CacheTiers {
            cold_end: indexes
                .range(..request.messages.len())
                .next_back()
                .map(|last| last + 1),
            previous_len: previous_len.filter(|_| classification == ContextClassification::Append),
        };
        let mut record = ContextBuild {
            request_kind: request_kind.to_owned(),
            frozen,
            cold,
            history,
            declared: reason.is_some(),
            declaration_reason: reason,
            invalid_cold_indexes,
            frozen_writer,
            seq: 0,
            since_previous_ms: None,
            classification,
        };
        let opening = log::Opening {
            session,
            lineage: request_kind,
            reads_from: request_kind,
            model: &request.model,
            reasoning_effort: request.reasoning_effort,
            unchanged_bytes: log::unchanged_bytes(
                &record.classification,
                previous_history,
                &measured,
            ),
            total_bytes: measured.total(),
        };
        let superseded = state.log.open(opening, &mut record, |frozen, cold| {
            tier_contents(&request, &indexes, request.messages.len(), frozen, cold)
        });
        drop(state);
        self.write_log(superseded);
        BuiltContext {
            tiers,
            request,
            record,
        }
    }

    /// Release deleted/expired sessions without retaining their diagnostic history.
    pub fn release_session(&self, session: &str) {
        let mut state = self.state.lock().unwrap();
        state.previous.retain(|(id, _), _| id != session);
        state.sessions.remove(session);
        state.tool_generations.remove(session);
        state.staged_notes.remove(session);
        state.served_notes.retain(|(id, _), _| id != session);
        state.preparations.retain(|(id, _), _| id != session);
        drop(state);
        self.release_log(session);
        self.reminders
            .lock()
            .unwrap()
            .retain(|(id, _), _| id != session);
    }
}

#[derive(Clone)]
struct Measured {
    frozen: TierFingerprint,
    cold: TierFingerprint,
    history: TierFingerprint,
    hashes: Vec<String>,
    /// Canonical bytes of each history message.
    sizes: Vec<usize>,
}
impl Measured {
    fn total(&self) -> usize {
        self.frozen.bytes + self.cold.bytes + self.history.bytes
    }
}

/// Tier fingerprints of the first `end` messages. Frozen is exactly system
/// prompt, tool declarations and disabled admission.
fn measure(request: &ModelRequest, indexes: &BTreeSet<usize>, end: usize) -> Measured {
    let frozen = tier(frozen_value(request), 0);
    let mut cold = Vec::new();
    let mut history = Vec::new();
    let mut hashes = Vec::new();
    let mut sizes = Vec::new();
    for (index, message) in request.messages.iter().take(end).enumerate() {
        let value = message_value(message);
        if indexes.contains(&index) {
            cold.push(json!({"index":index,"message":value}));
        } else {
            let bytes = canonical(value.clone());
            sizes.push(bytes.len());
            hashes.push(fingerprint(&bytes));
            history.push(value);
        }
    }
    Measured {
        frozen,
        cold: tier(json!(cold), cold.len()),
        history: tier(json!(history), history.len()),
        hashes,
        sizes,
    }
}

fn frozen_value(request: &ModelRequest) -> Value {
    json!({
        "system_prompt":request.system_prompt,
        "tools":tool_declarations(&request.tools),
        "disabled_tools":tool_declarations(&request.disabled_tools),
    })
}

/// Durable ids, timestamps and provenance are not model prompt content.
/// Provider replay state is prompt-affecting and remains in the hash.
fn message_value(message: &crate::Message) -> Value {
    json!({
        "role":message.role,"content":message.content,
        "tool_calls":message.tool_calls,"tool_call_id":message.tool_call_id,
        "is_error":message.is_error,"provider_state":message.provider_state,
    })
}

/// The exact hashed bytes of the frozen and cold tiers, for those that changed.
fn tier_contents(
    request: &ModelRequest,
    indexes: &BTreeSet<usize>,
    end: usize,
    frozen: bool,
    cold: bool,
) -> Vec<(String, String)> {
    let entry = |value: Value| {
        let text = String::from_utf8(canonical(value)).expect("canonical JSON is UTF-8");
        (fingerprint(text.as_bytes()), text)
    };
    let mut tiers = Vec::new();
    if frozen {
        tiers.push(entry(frozen_value(request)));
    }
    if cold {
        let messages = indexes.range(..end).filter_map(|&index| {
            let message = request.messages.get(index)?;
            Some(json!({"index":index,"message":message_value(message)}))
        });
        tiers.push(entry(json!(messages.collect::<Vec<_>>())));
    }
    tiers
}

/// Generation settings that must match before a side call can share a prefix.
fn settings(request: &ModelRequest) -> String {
    json!({
        "reasoning_effort":request.reasoning_effort,
        "max_output_tokens":request.max_output_tokens,
    })
    .to_string()
}

fn tier(value: Value, message_count: usize) -> TierFingerprint {
    let bytes = canonical(value);
    TierFingerprint {
        sha256: fingerprint(&bytes),
        bytes: bytes.len(),
        message_count,
    }
}

fn tool_declarations(tools: &[crate::ToolDefinition]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "name":tool.name,"description":tool.description,"input_schema":tool.input_schema,
            })
        })
        .collect()
}

/// Object keys are sorted recursively even when serde_json preserves insertion
/// order; array order, strings and numeric values are preserved.
fn canonical(value: Value) -> Vec<u8> {
    fn sorted(value: Value) -> Value {
        match value {
            Value::Object(map) => {
                let sorted: BTreeMap<_, _> = map
                    .into_iter()
                    .map(|(key, value)| (key, sorted(value)))
                    .collect();
                Value::Object(sorted.into_iter().collect())
            }
            Value::Array(values) => Value::Array(values.into_iter().map(sorted).collect()),
            value => value,
        }
    }
    serde_json::to_vec(&sorted(value)).expect("context content is JSON serializable")
}

#[cfg(test)]
mod tests {
    use super::ContextState;

    #[test]
    fn tool_generation_declares_a_later_change_once_per_session() {
        let state = ContextState::default();
        assert!(
            !state.observe_tool_generation("s", 0),
            "first observation never declares"
        );
        assert!(
            !state.observe_tool_generation("s", 0),
            "same generation never declares"
        );
        assert!(
            state.observe_tool_generation("s", 1),
            "a changed generation declares"
        );
        assert!(
            !state.observe_tool_generation("s", 1),
            "the change is declared once"
        );
        assert!(
            !state.observe_tool_generation("other", 1),
            "sessions are observed separately"
        );
    }
}
