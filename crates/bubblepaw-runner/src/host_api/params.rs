//! Reusable typed wire parameters. Unknown fields stay accepted wherever the
//! protocol 0.1 handler accepted them.
use bubblepaw_core::RunOptions;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionParams {
    pub session_id: String,
}
#[derive(Deserialize, Serialize)]
pub struct OpenParams {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default, alias = "name")]
    pub title: Option<String>,
    #[serde(default, alias = "parent_id")]
    pub parent_session_id: Option<String>,
}
#[derive(Deserialize, Serialize)]
pub struct RunParams {
    pub session_id: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub attachments: Vec<String>,
    #[serde(default)]
    pub options: RunOptions,
    #[serde(default)]
    pub script: Option<String>,
    #[serde(default)]
    pub client: Option<Value>,
}
#[derive(Deserialize, Serialize)]
pub struct TextParams {
    pub session_id: String,
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<String>,
    #[serde(default)]
    pub client: Option<Value>,
}
#[derive(Deserialize, Serialize)]
pub struct CancelParams {
    pub session_id: String,
    #[serde(default)]
    pub client: Option<Value>,
}
#[derive(Deserialize, Serialize)]
pub struct ChildParams {
    pub parent: String,
    pub child: String,
}
#[derive(Deserialize, Serialize)]
pub struct QuestionParams {
    pub session_id: String,
    pub question_id: String,
    pub text: String,
    #[serde(default)]
    pub client: Option<Value>,
}
#[derive(Deserialize, Serialize)]
pub struct ChildQuestionParams {
    pub parent: String,
    pub child: String,
    pub question_id: String,
    pub text: String,
}
#[derive(Deserialize, Serialize)]
pub struct ReadFileParams {
    pub session_id: String,
    pub id: String,
}
/// JSON transports encode binary bytes as base64; HTTP adapters can use this
/// same structure after decoding their binary request body.
#[derive(Deserialize, Serialize)]
pub struct AddFileParams {
    pub session_id: String,
    pub name: String,
    pub data: String,
}
#[derive(Deserialize, Serialize)]
pub struct EventsParams {
    #[serde(default)]
    pub after: Option<u64>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct EmptyParams {}
/// `config.get`: the whole effective settings list, or the one key (or section prefix) named.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ConfigGetParams {
    #[serde(default)]
    pub key: Option<String>,
}
/// `config.set`: one leaf key (`null` removes it so its default applies), or a
/// dynamic map entry (an object replaces the whole entry, `null` removes it).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSetParams {
    pub key: String,
    pub value: Value,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RenameParams {
    pub session_id: String,
    pub title: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RewindParams {
    pub session_id: String,
    pub message_id: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CompactParams {
    pub session_id: String,
    #[serde(default)]
    pub focus: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HostCommandParams {
    #[serde(default)]
    pub session_id: Option<String>,
    pub text: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DeleteResult {
    pub deleted: String,
    pub worktrees: Vec<crate::worktrees::CleanupReport>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RequestedResult {
    pub requested: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AnsweredResult {
    pub answered: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StoppedResult {
    pub stopped: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CompactResult {
    pub notice: Option<String>,
}
pub type RenameResult = bubblepaw_core::Session;
pub type RewindResult = crate::rewind::RewindResult;
/// Host commands return their command's result, null for ordinary text, or
/// `{needs_session:true}` when a recognized command needs a selected chat.
pub type HostCommandResult = Value;
