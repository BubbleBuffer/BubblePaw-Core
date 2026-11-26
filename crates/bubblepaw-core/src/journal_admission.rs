//! What run admission carries beside the run itself.
use crate::store::tool_surface::ChatToolSurface;
use crate::{ClientInfo, ContentBlock, ExecutionMode, MessageOrigin, Reminder, Run};

/// Writes that ride in the admission commit until each has its own seam.
pub(crate) struct Passengers<'a> {
    pub execution_mode: ExecutionMode,
    pub surface: Option<&'a ChatToolSurface>,
    pub reminders: &'a [Reminder],
    pub expected_wait: Option<&'a crate::QuotaWait>,
}

pub(crate) struct RunAdmission<'a> {
    pub run: &'a Run,
    /// The input this run answers; none for a run over retained history.
    pub content: Option<Vec<ContentBlock>>,
    pub origin: MessageOrigin,
    pub client: Option<ClientInfo>,
    /// A tool-only run answers no queued conversational input.
    pub tool_only: bool,
    pub passengers: Passengers<'a>,
}
