//! The notices the loop queues for each round: the tool budget, the tool
//! restriction and the rounds where tools are unavailable.
use bubblepaw_ai::ModelRequest;

/// Per-run tool budget reminder state: one start notice, then at most one
/// near-exhaustion warning. The kind is scoped to the run, so a later run with
/// the same limit still receives its own start notice.
#[derive(Default)]
pub(crate) struct BudgetReminder {
    pub start: Option<usize>,
    pub warned: bool,
}

/// Where tools stand for one model request of a run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ToolsRound {
    /// Tools are active.
    #[default]
    Open,
    /// Tools are unavailable, yet the request declares them exactly as before.
    /// Any call it returns is refused.
    Unavailable,
    /// The declarations move into `disabled_tools`, so the adapters send tool_choice none.
    Enforced,
}

impl ToolsRound {
    pub(crate) fn is_open(self) -> bool {
        self == Self::Open
    }

    /// Only the enforcement round moves the declarations out of `tools`.
    pub(crate) fn apply(self, request: &mut ModelRequest) {
        if self == Self::Enforced {
            request.disabled_tools = std::mem::take(&mut request.tools);
        }
    }
}

/// Per-run notices queued before each model request.
#[derive(Default)]
pub(crate) struct RunReminders {
    pub budget: BudgetReminder,
    pub restriction_sent: bool,
    /// Sticky: once tools are unavailable, they stay unavailable for the run.
    pub unavailable: bool,
    pub tools_off_reminded: bool,
    /// Set when an unavailable round returned a call; the next round enforces.
    pub enforce: bool,
    pub enforced: bool,
}

impl RunReminders {
    /// True when the next round is the enforcement round.
    pub(crate) fn enforcing(&self) -> bool {
        self.enforce && !self.enforced
    }
    /// A call returned on an unavailable round is refused, and enforcement follows.
    pub(crate) fn observe_tool_calls(&mut self, round: ToolsRound, calls: usize) {
        if round != ToolsRound::Open && calls > 0 {
            self.enforce = true;
        }
    }
}
