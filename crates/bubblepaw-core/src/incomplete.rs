//! Why an assistant reply holds only part of what the model was writing.
use serde::{Deserialize, Serialize};

/// The end of the request that streamed an incomplete assistant reply.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum IncompleteReason {
    /// The request failed after the text streamed.
    Error,
    /// The run was cancelled after the text streamed.
    Cancelled,
}
