//! The session shell writes things down through this trait: admission, queued
//! input and the end of a run. Each method is one atomic unit; the durable events
//! that belong to it are emitted by the implementation, in the same commit.
use crate::journal_admission::RunAdmission;
use crate::{ClientInfo, Message, QueuedInput, Run};
use anyhow::Result;

/// A run's final state, with the reply that settles it.
pub(crate) struct RunFinish<'a> {
    pub run: &'a Run,
    pub result_message_id: Option<&'a str>,
    pub final_message: Option<&'a Message>,
    pub origin: Option<ClientInfo>,
    /// The session settles with the run; false when a continuation follows.
    pub settles_session: bool,
}

pub(crate) trait SessionJournal: Send + Sync {
    /// Input accepted and the run admitted: run row, input, RunStarted.
    fn run_admitted(&self, admission: RunAdmission<'_>) -> Result<()>;
    /// Steering or follow-up input queued; announced only while a run is active.
    fn input_queued(
        &self,
        input: &QueuedInput,
        run_id: Option<&str>,
        client: Option<ClientInfo>,
    ) -> Result<()>;
    /// The run ended: reply, usage, run row, RunFinished and maybe SessionSettled.
    fn run_finished(&self, finish: RunFinish<'_>) -> Result<()>;
    /// A queued follow-up answered by a new run: run row, input, RunStarted.
    fn run_continued(&self, next: &Run) -> Result<()>;
    /// The session settled without a continuation having started.
    fn session_settled(&self, session_id: &str, run_id: &str);
}
