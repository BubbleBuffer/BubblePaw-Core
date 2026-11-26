//! The plain data of a reminder: what it says, which deliveries it carries and how it is recorded.
use serde::{Deserialize, Serialize};

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
            text_sha256: bubblepaw_ai::fingerprint(reminder.text.as_bytes()),
            deliveries: reminder.deliveries,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReminderMetadata {
    pub version: u32,
    pub request_kind: String,
    pub reminders: Vec<ReminderRecord>,
}
impl ReminderMetadata {
    pub fn new(request_kind: impl Into<String>, reminders: Vec<Reminder>) -> Self {
        Self {
            version: 1,
            request_kind: request_kind.into(),
            reminders: reminders.into_iter().map(Into::into).collect(),
        }
    }
}
