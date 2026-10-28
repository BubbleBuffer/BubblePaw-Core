//! Host-owned questions and user answers to a direct child's question.

pub use bubblepaw_runner::question::{ChildQuestionAnswerError, QuestionTools};
pub use bubblepaw_runner::wake::WakeCoordinator;

/// A child's pending question, ready for a host UI to display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingChildQuestion {
    pub id: String,
    pub text: String,
    /// Suggested answers; the user may always supply free text.
    pub options: Vec<String>,
    /// RFC 3339 timestamp; absent on records written by older hosts.
    pub asked_at: Option<String>,
    pub parent_session_id: String,
}
