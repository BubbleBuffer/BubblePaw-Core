//! Plain data about a run: its record and the explicit outputs it produces.
use bubblepaw_ai::Usage;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub session_id: String,
    pub model: String,
    pub status: String,
    pub text: Option<String>,
    pub error: Option<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub usage: Option<Usage>,
}

/// Explicit presentation output. The core stores data, never frontend code or binary payloads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputRecord {
    pub id: String,
    pub session_id: String,
    pub run_id: String,
    pub execution_id: String,
    pub created_at: String,
    pub output: Value,
}
