//! Cold notes: host instructions that depend on the applied view. They render
//! after the view, at the end of the cold tier, and change only when the view
//! is applied or another change is declared, so the cached prefix stays put.
use super::{ContextState, reminders::harness_block};
use crate::{Message, MessageOrigin, ModelRequest, Role, fingerprint};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ColdNote {
    kind: String,
    text: String,
}

impl ContextState {
    /// Stage a note for the session's cold tier. It reaches a lineage at that
    /// lineage's first build or at its next declared change, never in between.
    pub fn cold_note(&self, session: &str, kind: &str, text: &str) {
        let note = ColdNote {
            kind: kind.to_owned(),
            text: text.to_owned(),
        };
        let mut state = self.state.lock().unwrap();
        let notes = state.staged_notes.entry(session.to_owned()).or_default();
        if !notes.contains(&note) {
            notes.push(note);
        }
    }

    /// Drop the session's staged notes; served notes still wait for a declared change.
    pub fn clear_cold_notes(&self, session: &str) {
        self.state.lock().unwrap().staged_notes.remove(session);
    }

    /// Insert the lineage's notes right after its last cold message and return the
    /// cold indexes including the note message. A request without a cold tier
    /// carries no notes. Called before the request is validated and classified.
    pub fn place_cold_notes(
        &self,
        request: &mut ModelRequest,
        session: &str,
        request_kind: &str,
        cold_message_indexes: &[usize],
    ) -> Vec<usize> {
        let mut cold: Vec<_> = cold_message_indexes
            .iter()
            .copied()
            .filter(|index| *index < request.messages.len())
            .collect();
        cold.sort_unstable();
        cold.dedup();
        let key = (session.to_owned(), request_kind.to_owned());
        let mut state = self.state.lock().unwrap();
        let epoch = state.declaration(session).map_or(0, |(epoch, _)| epoch);
        let refresh = cold.is_empty()
            || state
                .previous
                .get(&key)
                .is_none_or(|previous| epoch > previous.declaration_epoch);
        if refresh {
            let staged = if cold.is_empty() {
                Vec::new()
            } else {
                state.staged_notes.get(session).cloned().unwrap_or_default()
            };
            state.served_notes.insert(key.clone(), staged);
        }
        let notes: Vec<_> = state
            .served_notes
            .get(&key)
            .into_iter()
            .flatten()
            .map(|note| (note.kind.clone(), note.text.clone()))
            .collect();
        drop(state);
        let Some(&last) = cold.last() else {
            return cold;
        };
        let created_at = request.messages[last].created_at.clone();
        if let Some(message) = Self::cold_notes_message(session, &notes, &created_at) {
            request.messages.insert(last + 1, message);
            cold.push(last + 1);
        }
        cold
    }

    /// One message with every note, ordered by kind and then insertion, in the same
    /// `<harness kind=…>` wrapper as reminders. Its id derives from its text.
    pub fn cold_notes_message(
        session: &str,
        notes: &[(String, String)],
        created_at: &str,
    ) -> Option<Message> {
        if notes.is_empty() {
            return None;
        }
        let mut ordered: Vec<_> = notes.iter().collect();
        ordered.sort_by(|a, b| a.0.cmp(&b.0));
        let content = ordered
            .iter()
            .map(|(kind, text)| harness_block(kind, text))
            .collect::<Vec<_>>()
            .join("\n");
        let mut message = crate::Store::message(session, Role::System, content);
        message.id = format!(
            "context:cold_notes:{}",
            fingerprint(message.content.as_bytes())
        );
        message.origin = Some(MessageOrigin::Generated);
        message.created_at = created_at.to_owned();
        Some(message)
    }
}
