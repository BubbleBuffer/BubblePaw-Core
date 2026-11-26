//! What a host's context preparation is asked for and what it returns.
use bubblepaw_ai::{ModelRequest, Usage};

/// Why the hook runs: an ordinary main request, or explicit manual compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrepareMode<'a> {
    Request,
    /// Retry final admission over fresh history, reserving space for additions
    /// which were absent from the host's prepared view. This is automatic
    /// pressure, not a user-requested manual compaction.
    Pressure {
        additional_tokens: usize,
        additional_bytes: usize,
    },
    Compact {
        focus: Option<&'a str>,
    },
}

#[derive(Debug, Clone)]
pub struct PreparedContext {
    pub request: ModelRequest,
    pub notice: Option<String>,
    pub usage: Option<Usage>,
}
