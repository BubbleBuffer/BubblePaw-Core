//! Typed, tail-only harness reminders. Accepted input and delivery identities are
//! durable; queue coalescing is disposable and never starts a provider request.
use super::ContextState;
use crate::{MessageOrigin, ModelRequest, Role, Store};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReminderDelivery {
    pub kind: String,
    pub id: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reminder {
    pub kind: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deliveries: Vec<ReminderDelivery>,
}
impl Reminder {
    pub fn new(kind: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            text: text.into(),
            deliveries: Vec::new(),
        }
    }
    pub fn with_delivery(mut self, kind: impl Into<String>, id: impl Into<String>) -> Self {
        self.deliveries.push(ReminderDelivery {
            kind: kind.into(),
            id: id.into(),
        });
        self
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReminderRecord {
    pub kind: String,
    pub text_sha256: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deliveries: Vec<ReminderDelivery>,
}
impl From<Reminder> for ReminderRecord {
    fn from(reminder: Reminder) -> Self {
        Self {
            kind: reminder.kind,
            text_sha256: crate::fingerprint(reminder.text.as_bytes()),
            deliveries: reminder.deliveries,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyReminderSource {
    pub record_id: String,
    pub text: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReminderMetadata {
    pub version: u32,
    pub request_kind: String,
    pub reminders: Vec<ReminderRecord>,
    /// Only original accepted input predating the immutable migration watermark.
    /// New message text is never a legacy delivery-identity source.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub legacy_sources: Vec<LegacyReminderSource>,
}
impl ReminderMetadata {
    pub fn new(request_kind: impl Into<String>, reminders: Vec<Reminder>) -> Self {
        Self {
            version: 1,
            request_kind: request_kind.into(),
            reminders: reminders.into_iter().map(Into::into).collect(),
            legacy_sources: Vec::new(),
        }
    }
}

#[derive(Default)]
pub(super) struct ReminderQueue {
    pending: Vec<Reminder>,
    sent: HashMap<String, String>,
    seeded: bool,
    loaded_inputs: HashSet<String>,
    direct_kinds: HashSet<String>,
    /// Kinds sent at the next flush even when their text is unchanged.
    fresh: HashSet<String>,
}
impl ReminderQueue {
    fn due(&self, reminder: &Reminder) -> bool {
        self.fresh.contains(&reminder.kind)
            || self.sent.get(&reminder.kind) != Some(&crate::fingerprint(reminder.text.as_bytes()))
    }
}
impl ContextState {
    pub(super) fn discard_pending_reminders(
        &self,
        session: &str,
        request_kind: &str,
        prefixes: &[&str],
    ) {
        let mut queues = self.reminders.lock().unwrap();
        let Some(queue) = queues.get_mut(&(session.into(), request_kind.into())) else {
            return;
        };
        let discarded: HashSet<_> = queue
            .pending
            .iter()
            .filter(|reminder| {
                reminder.deliveries.is_empty()
                    && queue.direct_kinds.contains(&reminder.kind)
                    && prefixes
                        .iter()
                        .any(|prefix| reminder.kind.starts_with(prefix))
            })
            .map(|reminder| reminder.kind.clone())
            .collect();
        queue
            .pending
            .retain(|reminder| !discarded.contains(&reminder.kind));
        queue.fresh.retain(|kind| !discarded.contains(kind));
        queue.direct_kinds.retain(|kind| !discarded.contains(kind));
    }

    /// Queue replaceable, unsent host facts: the latest text per kind wins.
    /// Persistence begins at flush; typed accepted user input is unaffected.
    /// Discrete delivery events must use identity-scoped kinds.
    pub fn remind(&self, session: &str, request_kind: &str, kind: &str, text: &str) {
        self.remind_with_metadata(session, request_kind, Reminder::new(kind, text));
    }
    pub fn remind_with_metadata(&self, session: &str, request_kind: &str, reminder: Reminder) {
        let mut queues = self.reminders.lock().unwrap();
        let queue = queues
            .entry((session.into(), request_kind.into()))
            .or_default();
        queue.direct_kinds.insert(reminder.kind.clone());
        if let Some(pending) = queue
            .pending
            .iter_mut()
            .find(|pending| pending.kind == reminder.kind)
        {
            *pending = reminder;
        } else {
            queue.pending.push(reminder);
        }
    }

    /// Queue a fact that must be sent again although its text is unchanged:
    /// the earlier copy left the model's view, for example at an LCM apply.
    pub fn remind_fresh(&self, session: &str, request_kind: &str, kind: &str, text: &str) {
        self.remind(session, request_kind, kind, text);
        let mut queues = self.reminders.lock().unwrap();
        let queue = queues
            .entry((session.into(), request_kind.into()))
            .or_default();
        queue.fresh.insert(kind.to_owned());
    }

    pub(crate) fn preview_reminder_message(
        &self,
        session: &str,
        request_kind: &str,
        store: &Store,
    ) -> Result<Option<crate::Message>> {
        self.load_pending(session, request_kind, store)?;
        let queues = self.reminders.lock().unwrap();
        let queue = &queues[&(session.into(), request_kind.into())];
        let batch: Vec<_> = queue
            .pending
            .iter()
            .filter(|reminder| queue.due(reminder))
            .cloned()
            .collect();
        Ok(reminder_message(session, &batch))
    }

    fn load_pending(
        &self,
        session: &str,
        request_kind: &str,
        store: &Store,
    ) -> Result<Vec<String>> {
        let inputs = store.pending_reminder_inputs(session, request_kind)?;
        let mut queues = self.reminders.lock().unwrap();
        let queue = queues
            .entry((session.into(), request_kind.into()))
            .or_default();
        // A durable source is loaded once. Replaying it during final build must
        // not replace a newer host fact queued after the admission preview.
        // Direct host facts also win the first-load collision with staged input.
        for (id, reminder) in &inputs {
            if queue.loaded_inputs.insert(id.clone())
                && !queue.direct_kinds.contains(&reminder.kind)
            {
                if let Some(pending) = queue
                    .pending
                    .iter_mut()
                    .find(|pending| pending.kind == reminder.kind)
                {
                    *pending = reminder.clone();
                } else {
                    queue.pending.push(reminder.clone());
                }
            }
        }
        if !queue.seeded {
            queue.sent = store
                .last_reminders(session, request_kind)?
                .into_iter()
                .map(|reminder| (reminder.kind, reminder.text_sha256))
                .collect();
            queue.seeded = true;
        }
        Ok(inputs.into_iter().map(|(id, _)| id).collect())
    }

    /// Flush a validated durable tail, then invoke the ordinary classifier once.
    /// Validation/storage failures retain the queue and do not advance hashes or
    /// last-sent values. Existing history messages are never moved or restored.
    #[allow(clippy::too_many_arguments)]
    pub fn build_persisted(
        &self,
        mut request: ModelRequest,
        session: &str,
        request_kind: &str,
        cold_message_indexes: &[usize],
        store: &Store,
        validate: impl FnOnce(&mut ModelRequest) -> Result<()>,
    ) -> Result<super::BuiltContext> {
        let cold_message_indexes =
            &self.place_cold_notes(&mut request, session, request_kind, cold_message_indexes);
        let input_ids = self.load_pending(session, request_kind, store)?;
        let mut queues = self.reminders.lock().unwrap();
        let queue = queues
            .entry((session.into(), request_kind.into()))
            .or_default();
        let batch: Vec<_> = queue
            .pending
            .iter()
            .filter(|reminder| queue.due(reminder))
            .cloned()
            .collect();
        let message = reminder_message(session, &batch);
        if let Some(message) = &message {
            request.messages.push(message.clone());
        }
        validate(&mut request)?;
        if let Some(message) = message {
            store.append_reminder_message(
                &message,
                &ReminderMetadata::new(request_kind, batch.clone()),
                &input_ids,
            )?;
        } else {
            store.acknowledge_reminder_inputs(session, &input_ids)?;
        }
        for reminder in batch {
            queue
                .sent
                .insert(reminder.kind, crate::fingerprint(reminder.text.as_bytes()));
        }
        queue.pending.clear();
        queue.fresh.clear();
        queue.loaded_inputs.clear();
        queue.direct_kinds.clear();
        drop(queues);
        Ok(self.build(request, session, request_kind, cold_message_indexes))
    }
}
pub(super) fn reminder_message(session: &str, reminders: &[Reminder]) -> Option<crate::Message> {
    if reminders.is_empty() {
        return None;
    }
    let content = reminders
        .iter()
        .map(|reminder| harness_block(&reminder.kind, &reminder.text))
        .collect::<Vec<_>>()
        .join("\n");
    let mut message = Store::message(session, Role::User, content);
    message.origin = Some(MessageOrigin::Generated);
    Some(message)
}
/// The wrapper shared by tail reminders and cold notes.
pub(super) fn harness_block(kind: &str, text: &str) -> String {
    format!("<harness kind=\"{}\">{text}</harness>", attribute(kind))
}
fn attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
#[path = "reminder_queue_tests.rs"]
mod tests;
