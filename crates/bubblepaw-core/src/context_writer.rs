//! The host's narrow writer into ContextState, handed to
//! `ContextPreparation::prepare` through `PrepareCall`. Dynamic per-request
//! context goes to the tail (reminders) or the end of the cold tier (cold
//! notes); the system prompt and tool declarations stay frozen.
use super::{ContextState, Previous};

/// Writes the root lineage of one session. Reminders are deduped per kind and
/// sent only when their text changed; cold notes are staged for this build.
pub struct ContextWriter<'a> {
    state: &'a ContextState,
    session: &'a str,
}
impl<'a> ContextWriter<'a> {
    pub fn new(state: &'a ContextState, session: &'a str) -> Self {
        Self { state, session }
    }
    /// Queue `<harness kind="KIND">TEXT</harness>` at the request tail.
    pub fn remind(&self, kind: &str, text: &str) {
        self.state.remind(self.session, "root", kind, text);
    }
    /// Queue a reminder that is sent once even when its text is unchanged, for a
    /// fact whose earlier copy left the model's view (see `ContextState::remind_fresh`).
    pub fn remind_fresh(&self, kind: &str, text: &str) {
        self.state.remind_fresh(self.session, "root", kind, text);
    }
    /// Discard unsent host facts already represented by a successfully committed
    /// snapshot. Accepted inputs and reminders carrying delivery identities stay.
    pub fn discard_pending_reminders(&self, prefixes: &[&str]) {
        self.state
            .discard_pending_reminders(self.session, "root", prefixes);
    }
    /// Stage a note for the end of the cold tier (see ContextState::cold_note).
    pub fn cold_note(&self, kind: &str, text: &str) {
        self.state.cold_note(self.session, kind, text);
    }
}

impl ContextState {
    /// Record the frozen tier before and after the host's preparation hook for
    /// the lineage's next build, so a change the hook made can be told apart
    /// from a harness change (a resolver or tool reload declares its own).
    pub fn note_preparation(
        &self,
        session: &str,
        request_kind: &str,
        before: &crate::ModelRequest,
        after: &crate::ModelRequest,
    ) {
        let pair = (frozen_sha(before), frozen_sha(after));
        self.state
            .lock()
            .unwrap()
            .preparations
            .insert((session.to_owned(), request_kind.to_owned()), pair);
    }
}

fn frozen_sha(request: &crate::ModelRequest) -> String {
    super::measure(request, &Default::default(), 0)
        .frozen
        .sha256
}

/// The hook changed the frozen tier: its input matches the lineage's previous
/// build but its output does not.
pub(super) fn hook_changed_frozen(
    previous: Option<&Previous>,
    current: Option<&(String, String)>,
) -> bool {
    match (
        previous.and_then(|before| before.preparation.as_ref()),
        current,
    ) {
        (Some((input, output)), Some((now_input, now_output))) => {
            input == now_input && output != now_output
        }
        _ => false,
    }
}
