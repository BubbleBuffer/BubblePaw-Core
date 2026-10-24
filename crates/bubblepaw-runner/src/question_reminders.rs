//! Question delivery uses persisted identities rather than rendered answer markers.
use super::*;

impl QuestionAnswer {
    pub(super) fn input_reminder(&self) -> bubblepaw_core::Reminder {
        let id = self.question["id"].as_str().unwrap_or_default();
        bubblepaw_core::Reminder::new(
            format!("question_answer:{id}"),
            format!(
                "Question: {}\nAnswer: {}",
                self.question["text"].as_str().unwrap_or_default(),
                self.question["answer"].as_str().unwrap_or_default()
            ),
        )
        .with_delivery("question_answer", id)
    }
    pub(crate) fn continuation_reminder(&self) -> bubblepaw_core::Reminder {
        let mut reminder = self.input_reminder();
        reminder.text = format!(
            "The pending question was answered after its original tool execution ended. Continue from this answer; do not replay the interrupted JS cell.\n{}",
            reminder.text
        );
        reminder
    }
}
impl QuestionTools {
    pub(super) fn take_reminders(&self, session: &str) -> Vec<bubblepaw_core::Reminder> {
        let mut reminders = self.wake.take_reminders(session, "question:");
        match self.unrecorded_answer(session) {
            Ok(Some(answer)) => reminders.push(answer.input_reminder()),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%session, %error, "saved question answer could not be recovered")
            }
        }
        reminders
    }
}

fn answer_marker(answer: &QuestionAnswer) -> crate::wake::DeliveryMarker {
    let id = answer.question["id"].as_str().unwrap_or_default();
    crate::wake::DeliveryMarker::new("question_answer", id, format!("[question answer:{id}]"))
}
pub(crate) fn answer_is_undelivered(
    store: &Store,
    session: &str,
    answer: &QuestionAnswer,
) -> Result<bool> {
    Ok(crate::wake::undelivered_markers(store, session, [answer_marker(answer)])?.len() == 1)
}
pub(crate) fn answer_has_pending_input(
    store: &Store,
    session: &str,
    answer: &QuestionAnswer,
) -> Result<bool> {
    let marker = answer_marker(answer);
    for input in store.queued_inputs(session)? {
        if matches!(input.state.as_str(), "pending" | "reminder_pending")
            && crate::wake::record_delivers(
                store,
                session,
                &input.id,
                input.origin,
                &input.text,
                &marker,
            )?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Accept the recovered answer as new generated input. Interrupted tool cells
/// and unresolved external effects remain intact and are never re-executed.
pub fn continue_answer(
    harness: &Arc<Harness>,
    store: &Store,
    session: &str,
    answer: &QuestionAnswer,
    client: Option<ClientInfo>,
) -> Result<()> {
    static ADMISSION: Mutex<()> = Mutex::new(());
    let _admission = ADMISSION
        .lock()
        .map_err(|_| anyhow!("question admission poisoned"))?;
    if !answer.live {
        if answer_has_pending_input(store, session, answer)? {
            ensure!(
                store.active_session_metadata(session)?.is_some(),
                "session is closed or archived"
            );
            harness.start_resume(
                session,
                RunOptions {
                    origin: MessageOrigin::Generated,
                    client,
                    ..Default::default()
                },
            )?;
        } else if answer_is_undelivered(store, session, answer)? {
            harness.start_reminders_if_active(
                session,
                vec![answer.continuation_reminder()],
                RunOptions {
                    origin: MessageOrigin::Generated,
                    client,
                    ..Default::default()
                },
            )?;
        }
    }
    Ok(())
}
