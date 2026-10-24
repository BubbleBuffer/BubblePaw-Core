//! Provider extension types (docs/providers.md): capabilities, the history
//! cursor and typed provider errors. Everything here has defaults so a provider
//! that only implements `generate` keeps working unchanged.
use crate::{Message, ModelRequest, ToolRegistry, ToolRegistryResolver, Usage};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

impl ModelRequest {
    pub fn validate_output_schema(&self) -> anyhow::Result<()> {
        if let Some(schema) = &self.output_schema {
            schema.validate()?;
        }
        Ok(())
    }
}

/// Tells a stateful provider whether the prepared history is a pure append of
/// the previous step (same epoch) or was rewritten from a given message on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryCursor {
    pub epoch: String,
    pub rewritten_from: Option<String>,
}
impl HistoryCursor {
    /// A new epoch with no reusable prefix: the provider rebuilds everything.
    pub fn fresh() -> Self {
        Self {
            epoch: crate::id(),
            rewritten_from: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheMode {
    #[default]
    None,
    /// The request prefix is cached server side.
    Prefix,
    /// The provider keeps a stateful mirror of the session.
    Session,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ToolSurface {
    /// Every model tool is offered as a tool.
    #[default]
    Direct,
    /// Only `js` is offered; every other tool lives in `tools.*` inside it,
    /// discovered with `tools.$search`.
    Code,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    #[serde(default)]
    pub context_window: Option<usize>,
    #[serde(default)]
    pub cache: CacheMode,
    #[serde(default)]
    pub tool_surface: ToolSurface,
    #[serde(default)]
    pub stateful: bool,
    /// Whether this selected model accepts image input from the host.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub image_input: bool,
}

/// Prefix of a prepared message id that marks per-step context (a runtime note,
/// for instance) rather than durable history. The history cursor ignores such
/// messages, and stateful providers must not treat them as part of their mirror.
pub const EPHEMERAL_ID_PREFIX: &str = "ephemeral:";

/// Whether a prepared message is per-step context (see [`EPHEMERAL_ID_PREFIX`]).
pub fn is_ephemeral(message: &Message) -> bool {
    message.id.starts_with(EPHEMERAL_ID_PREFIX)
}

/// The code-mode entry point, the only tool a `Code` surface offers.
pub const CODE_TOOL: &str = "js";

/// Apply a provider's tool surface to one prepared request. Registration and
/// direct-call validation are unaffected; only the advertised list changes.
pub fn apply_tool_surface(request: &mut ModelRequest, surface: &ToolSurface) {
    // Without `js` among the request's tools, Code falls back to Direct.
    if *surface == ToolSurface::Code && request.tools.iter().any(|tool| tool.name == CODE_TOOL) {
        request.tools.retain(|tool| tool.name == CODE_TOOL);
    }
}

/// Wrap a source resolver so each snapshot passes through `derive`. The
/// closure runs after `source()`, once per resolution.
pub fn derived_registry_resolver(
    source: ToolRegistryResolver,
    derive: impl Fn(ToolRegistry) -> ToolRegistry + Send + Sync + 'static,
) -> ToolRegistryResolver {
    Arc::new(move || derive(source()))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderErrorKind {
    Auth,
    RateLimit {
        /// RFC 3339.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resets_at: Option<String>,
    },
    Context,
    Transient,
    Fatal,
}

/// A classified provider failure. Providers return it inside `anyhow::Error`;
/// hosts recover it with [`ProviderError::of`]. Display is the message only.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub message: String,
    /// Text has reached the listener, so retrying could replay published output.
    pub published: bool,
    /// Known billable usage, even when the response cannot be accepted.
    pub usage: Option<Usage>,
    pub quota: Vec<crate::QuotaReading>,
}
impl ProviderError {
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            published: false,
            usage: None,
            quota: Vec::new(),
        }
    }
    pub fn with_published(mut self, published: bool) -> Self {
        self.published = published;
        self
    }
    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = Some(usage);
        self
    }
    pub fn with_quota(mut self, quota: Vec<crate::QuotaReading>) -> Self {
        self.quota = quota;
        self
    }
    /// Whether this failure is safe to retry before any output was published.
    pub fn retryable(&self) -> bool {
        !self.published
            && matches!(
                self.kind,
                ProviderErrorKind::Transient | ProviderErrorKind::RateLimit { .. }
            )
    }
    pub fn of(error: &anyhow::Error) -> Option<&ProviderError> {
        error
            .downcast_ref()
            .or_else(|| error.chain().find_map(|cause| cause.downcast_ref()))
    }
}
impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ProviderError {}

/// Whether an error chain contains a provider failure that is safe to retry.
pub fn provider_error_retryable(error: &anyhow::Error) -> bool {
    ProviderError::of(error).is_some_and(ProviderError::retryable)
}

/// Per-session memory of the previous step's prepared messages, used by the
/// harness to compute the cursor centrally after context preparation.
#[derive(Default)]
pub struct HistoryTracker {
    sessions: Mutex<HashMap<String, (String, Vec<Fingerprint>)>>,
}
/// A prepared message as `(id, content hash)`.
type Fingerprint = (String, [u8; 32]);
impl HistoryTracker {
    /// Record this step's messages and return its cursor. Ephemeral messages
    /// are left out entirely: they are neither remembered nor compared.
    pub fn advance(&self, session_id: &str, messages: &[Message]) -> HistoryCursor {
        let current: Vec<Fingerprint> = messages
            .iter()
            .filter(|message| !is_ephemeral(message))
            .map(|message| (message.id.clone(), hash(message)))
            .collect();
        let mut sessions = self.sessions.lock().unwrap();
        let cursor = match sessions.get(session_id) {
            Some((epoch, previous))
                if previous.len() <= current.len() && current.starts_with(previous) =>
            {
                HistoryCursor {
                    epoch: epoch.clone(),
                    rewritten_from: None,
                }
            }
            Some((_, previous)) => {
                // The first differing position. A pure truncation has no differing
                // message in the new list, so it rebuilds from the start.
                let first = previous
                    .iter()
                    .zip(&current)
                    .position(|(old, new)| old != new);
                HistoryCursor {
                    epoch: crate::id(),
                    rewritten_from: first
                        .filter(|index| *index > 0)
                        .map(|index| current[index].0.clone()),
                }
            }
            None => HistoryCursor::fresh(),
        };
        sessions.insert(session_id.into(), (cursor.epoch.clone(), current));
        cursor
    }
    pub fn forget(&self, session_id: &str) {
        self.sessions.lock().unwrap().remove(session_id);
    }
}
fn hash(message: &Message) -> [u8; 32] {
    Sha256::digest(serde_json::to_vec(message).unwrap_or_default()).into()
}

/// Lets a provider host hand a session a fresh cursor (after a provider switch
/// or an extension restart) without forcing a second rebuild on the next
/// append: later steps of the same harness epoch keep the substituted epoch.
#[derive(Default)]
pub struct CursorResets {
    sessions: Mutex<HashMap<String, (String, String)>>,
}
impl CursorResets {
    pub fn new() -> Self {
        Self::default()
    }
    /// Replace the cursor by a fresh one and remember the substitution.
    pub fn reset(&self, session_id: &str, cursor: &mut HistoryCursor) {
        let fresh = HistoryCursor::fresh();
        self.sessions.lock().unwrap().insert(
            session_id.into(),
            (cursor.epoch.clone(), fresh.epoch.clone()),
        );
        *cursor = fresh;
    }
    /// Keep presenting a substituted epoch while the harness epoch is unchanged.
    pub fn apply(&self, session_id: &str, cursor: &mut HistoryCursor) {
        let mut sessions = self.sessions.lock().unwrap();
        match sessions.get(session_id) {
            Some((original, alias)) if *original == cursor.epoch => {
                cursor.epoch = alias.clone();
            }
            Some(_) => {
                sessions.remove(session_id);
            }
            None => {}
        }
    }
    pub fn forget(&self, session_id: &str) {
        self.sessions.lock().unwrap().remove(session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Role, Store, ToolDefinition};

    fn message(id: &str, content: &str) -> Message {
        let mut message = Store::message("s", Role::User, content.to_string());
        message.id = id.into();
        message.created_at = "t".into();
        message
    }

    #[test]
    fn first_step_append_rewrite_and_first_message_change() {
        let tracker = HistoryTracker::default();
        let a = message("a", "one");
        let b = message("b", "two");
        let c = message("c", "three");
        let first = tracker.advance("s", std::slice::from_ref(&a));
        assert!(!first.epoch.is_empty());
        assert_eq!(first.rewritten_from, None);
        let append = tracker.advance("s", &[a.clone(), b.clone()]);
        assert_eq!(append, first);
        let same = tracker.advance("s", &[a.clone(), b.clone()]);
        assert_eq!(same, first);
        let mut edited = b.clone();
        edited.content = "two, edited".into();
        let rewrite = tracker.advance("s", &[a.clone(), edited.clone(), c.clone()]);
        assert_ne!(rewrite.epoch, first.epoch);
        assert_eq!(rewrite.rewritten_from.as_deref(), Some("b"));
        let replaced = tracker.advance("s", &[a.clone(), c.clone()]);
        assert_eq!(replaced.rewritten_from.as_deref(), Some("c"));
        let front = tracker.advance("s", &[message("z", "summary"), c.clone()]);
        assert_ne!(front.epoch, replaced.epoch);
        assert_eq!(front.rewritten_from, None);
        let truncated = tracker.advance("s", &[message("z", "summary")]);
        assert_ne!(truncated.epoch, front.epoch);
        assert_eq!(truncated.rewritten_from, None);
        let other = tracker.advance("other", &[a]);
        assert_ne!(other.epoch, truncated.epoch);
        assert_eq!(other.rewritten_from, None);
    }

    #[test]
    fn ephemeral_messages_do_not_affect_the_cursor() {
        let tracker = HistoryTracker::default();
        let summary = message("context:summary:c1", "chapters");
        let a = message("a", "one");
        let note = |content: &str| message("ephemeral:runtime-note", content);
        let first = tracker.advance("s", &[summary.clone(), a.clone(), note("jobs: 1")]);
        // A changing trailing note keeps the epoch.
        let b = message("b", "two");
        let second = tracker.advance(
            "s",
            &[summary.clone(), a.clone(), b.clone(), note("jobs: 2")],
        );
        assert_eq!(second, first);
        // An ephemeral message in the middle, or none at all, changes nothing either.
        let third = tracker.advance("s", &[summary.clone(), note("x"), a.clone(), b.clone()]);
        assert_eq!(third, first);
        // A rewrite never names an ephemeral message.
        let mut edited = b.clone();
        edited.content = "two, edited".into();
        let rewrite = tracker.advance("s", &[summary.clone(), a.clone(), note("y"), edited]);
        assert_ne!(rewrite.epoch, first.epoch);
        assert_eq!(rewrite.rewritten_from.as_deref(), Some("b"));
        assert!(is_ephemeral(&note("z")));
        assert!(!is_ephemeral(&summary));
    }

    #[test]
    fn a_stable_summary_block_keeps_the_epoch_and_a_new_one_rewrites() {
        let tracker = HistoryTracker::default();
        let summary = message("context:summary:c1", "chapter 1");
        let a = message("a", "one");
        let b = message("b", "two");
        let first = tracker.advance("s", &[summary.clone(), a.clone()]);
        let next = tracker.advance("s", &[summary.clone(), a.clone(), b.clone()]);
        assert_eq!(next, first);
        // A new chapter changes the block's id: a rewrite from the front.
        let newer = tracker.advance("s", &[message("context:summary:c2", "chapters 1-2"), b]);
        assert_ne!(newer.epoch, first.epoch);
        assert_eq!(newer.rewritten_from, None);
    }

    #[test]
    fn resets_alias_the_harness_epoch_until_it_changes() {
        let resets = CursorResets::new();
        let mut cursor = HistoryCursor {
            epoch: "e1".into(),
            rewritten_from: Some("m".into()),
        };
        resets.reset("s", &mut cursor);
        assert_ne!(cursor.epoch, "e1");
        assert_eq!(cursor.rewritten_from, None);
        let alias = cursor.epoch.clone();
        let mut next = HistoryCursor {
            epoch: "e1".into(),
            rewritten_from: None,
        };
        resets.apply("s", &mut next);
        assert_eq!(next.epoch, alias);
        let mut changed = HistoryCursor {
            epoch: "e2".into(),
            rewritten_from: Some("x".into()),
        };
        resets.apply("s", &mut changed);
        assert_eq!(changed.epoch, "e2");
        let mut again = HistoryCursor {
            epoch: "e1".into(),
            rewritten_from: None,
        };
        resets.apply("s", &mut again);
        assert_eq!(again.epoch, "e1");
    }

    #[test]
    fn serde_shapes_match_the_spec() {
        assert_eq!(
            serde_json::to_value(Capabilities {
                context_window: Some(10),
                cache: CacheMode::Session,
                tool_surface: ToolSurface::Code,
                stateful: true,
                image_input: false,
            })
            .unwrap(),
            serde_json::json!({"context_window":10,"cache":"session","tool_surface":{"kind":"code"},"stateful":true})
        );
        assert_eq!(
            serde_json::to_value(ToolSurface::Direct).unwrap(),
            serde_json::json!({"kind":"direct"})
        );
        assert_eq!(
            serde_json::to_value(ProviderErrorKind::RateLimit {
                resets_at: Some("2026-10-08T14:00:00Z".into())
            })
            .unwrap(),
            serde_json::json!({"kind":"rate_limit","resets_at":"2026-10-08T14:00:00Z"})
        );
        assert_eq!(
            serde_json::from_value::<ProviderErrorKind>(serde_json::json!({"kind":"rate_limit"}))
                .unwrap(),
            ProviderErrorKind::RateLimit { resets_at: None }
        );
        let error = anyhow::Error::new(ProviderError::new(ProviderErrorKind::Auth, "sign in"))
            .context("outer");
        assert_eq!(
            ProviderError::of(&error).unwrap().kind,
            ProviderErrorKind::Auth
        );
        assert!(ProviderError::of(&anyhow::anyhow!("plain")).is_none());
        let request: ModelRequest = serde_json::from_value(serde_json::json!({
            "model":"m","system_prompt":"s","messages":[],"tools":[],"max_output_tokens":null
        }))
        .unwrap();
        assert_eq!(request.history, HistoryCursor::default());
        assert!(request.session_id.is_empty());
    }

    #[test]
    fn retryability_requires_a_retryable_kind_and_no_published_output() {
        for kind in [
            ProviderErrorKind::Transient,
            ProviderErrorKind::RateLimit { resets_at: None },
            ProviderErrorKind::Auth,
            ProviderErrorKind::Context,
            ProviderErrorKind::Fatal,
        ] {
            let expected = matches!(
                kind,
                ProviderErrorKind::Transient | ProviderErrorKind::RateLimit { .. }
            );
            let error = ProviderError::new(kind.clone(), "synthetic failure");
            assert!(!error.published);
            assert_eq!(error.retryable(), expected);
            let wrapped = anyhow::Error::new(error.clone()).context("host context");
            assert_eq!(provider_error_retryable(&wrapped), expected);

            let published = error.with_published(true);
            assert_eq!(published.kind, kind);
            assert_eq!(published.to_string(), "synthetic failure");
            assert!(!published.retryable());
            let wrapped = anyhow::Error::new(published).context("host context");
            assert!(ProviderError::of(&wrapped).unwrap().published);
            assert!(!provider_error_retryable(&wrapped));
        }
        assert!(!provider_error_retryable(&anyhow::anyhow!("unclassified")));
        let error = anyhow::Error::new(ProviderError::new(
            ProviderErrorKind::Transient,
            "synthetic failure",
        ))
        .context(
            ProviderError::new(ProviderErrorKind::Transient, "synthetic failure")
                .with_published(true),
        )
        .context("host context");
        assert!(ProviderError::of(&error).unwrap().published);
        assert!(!provider_error_retryable(&error));
    }

    #[test]
    fn code_surface_keeps_only_js_and_falls_back_without_it() {
        let tool = |name: &str| ToolDefinition {
            approval: Default::default(),
            name: name.into(),
            description: String::new(),
            input_schema: serde_json::json!({"type":"object"}),
            read_only: true,
        };
        let mut request = ModelRequest {
            tools: vec![tool("js"), tool("history_search"), tool("other")],
            ..Default::default()
        };
        let surface = ToolSurface::Code;
        apply_tool_surface(&mut request, &ToolSurface::Direct);
        assert_eq!(request.tools.len(), 3);
        apply_tool_surface(&mut request, &surface);
        let names: Vec<_> = request.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["js"]);
        let mut without_js = ModelRequest {
            tools: vec![tool("history_search"), tool("other")],
            ..Default::default()
        };
        apply_tool_surface(&mut without_js, &surface);
        assert_eq!(without_js.tools.len(), 2);
    }
}
