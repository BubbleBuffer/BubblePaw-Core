//! Explicit host wake admission. Merely queuing a ContextState reminder never
//! starts a run; these APIs retain the host's existing deliberate wake decision.
use super::*;
use crate::{Reminder, ReminderMetadata};

/// Per-run tool budget reminder state: one start notice, then at most one
/// near-exhaustion warning. The kind is scoped to the run, so a later run with
/// the same limit still receives its own start notice.
#[derive(Default)]
pub(super) struct BudgetReminder {
    start: Option<usize>,
    warned: bool,
}

/// Where tools stand for one model request of a run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum ToolsRound {
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
    pub(super) fn is_open(self) -> bool {
        self == Self::Open
    }

    /// Only the enforcement round moves the declarations out of `tools`.
    pub(super) fn apply(self, request: &mut crate::ModelRequest) {
        if self == Self::Enforced {
            request.disabled_tools = std::mem::take(&mut request.tools);
        }
    }
}

/// Per-run harness notices queued before each model request.
#[derive(Default)]
pub(super) struct RunReminders {
    budget: BudgetReminder,
    restriction_sent: bool,
    /// Sticky: once tools are unavailable, they stay unavailable for the run.
    unavailable: bool,
    tools_off_reminded: bool,
    /// Set when an unavailable round returned a call; the next round enforces.
    enforce: bool,
    enforced: bool,
}

impl RunReminders {
    /// True when the next round is the enforcement round.
    pub(super) fn enforcing(&self) -> bool {
        self.enforce && !self.enforced
    }
    /// A call returned on an unavailable round is refused, and enforcement follows.
    pub(super) fn observe_tool_calls(&mut self, round: ToolsRound, calls: usize) {
        if round != ToolsRound::Open && calls > 0 {
            self.enforce = true;
        }
    }
}

impl Harness {
    /// Queue the run's notices and decide the tools of the next model request. The
    /// tool restriction is stated once, at the run's first model request, and only
    /// when `tool_names` is set.
    pub(super) fn queue_run_reminders(
        &self,
        run: &Run,
        options: &RunOptions,
        dispatcher: &ToolDispatcher,
        state: &mut RunReminders,
    ) -> ToolsRound {
        if let Some(limit) = options.max_tool_calls {
            let left = dispatcher.remaining_tool_calls().unwrap_or(limit);
            self.queue_budget_reminder(&run.session_id, &run.id, limit, left, &mut state.budget);
        }
        if let Some(names) = options
            .tool_names
            .as_deref()
            .filter(|_| !state.restriction_sent)
        {
            state.restriction_sent = true;
            let mut names = names.to_vec();
            if options.output_schema.is_some() {
                names.push(crate::submit_result::NAME.into());
            }
            let text = format!(
                "Tool calls are restricted in this run. Allowed tools: {}. A call to any other tool returns an error result without running.",
                crate::tool_views::allowed_tools_phrase(&names)
            );
            self.context_state().remind(
                &run.session_id,
                "root",
                &format!("tool_restriction:{}", run.id),
                &text,
            );
        }
        state.unavailable |= dispatcher.remaining_tool_calls() == Some(0);
        if !state.unavailable {
            return ToolsRound::Open;
        }
        if options.output_schema.is_some() {
            if !state.tools_off_reminded {
                state.tools_off_reminded = true;
                self.context_state().remind(
                    &run.session_id, "root", &format!("tools_off:{}", run.id),
                    "The work tool budget is exhausted. Ordinary tool calls are unavailable. Complete this run with submit_result; it remains available directly or through the tool wrapper.",
                );
            }
            return ToolsRound::Open;
        }
        if state.enforce {
            if !state.enforced {
                state.enforced = true;
                self.declare_context_change(&run.session_id, "tools_off_enforced");
            }
            return ToolsRound::Enforced;
        }
        // The first unavailable round keeps the declarations and asks the model to answer in text.
        if !state.tools_off_reminded {
            state.tools_off_reminded = true;
            self.context_state().remind(
                &run.session_id,
                "root",
                &format!("tools_off:{}", run.id),
                "Tool calls are unavailable for the rest of this run. Answer in text using the information you already have.",
            );
        }
        ToolsRound::Unavailable
    }
    pub(super) fn queue_budget_reminder(
        &self,
        session: &str,
        run: &str,
        limit: usize,
        remaining: usize,
        state: &mut BudgetReminder,
    ) {
        let kind = format!("tool_budget:{run}");
        let text = match state.start {
            None => {
                state.start = Some(remaining);
                format!(
                    "You have {limit} tool calls for this run. Model tool calls and each nested host tool call all count toward this single budget. {remaining} tool calls left. Once exhausted, finish using the information already available."
                )
            }
            Some(start)
                if !state.warned && remaining < start && remaining <= (limit / 5).max(2) =>
            {
                state.warned = true;
                format!(
                    "Tool budget nearly exhausted: {remaining} of {limit} tool calls left for this run. Once exhausted, finish using the information already available."
                )
            }
            Some(_) => return,
        };
        self.context_state().remind(session, "root", &kind, &text);
    }
    pub fn start_reminders(
        &self,
        session: &str,
        reminders: Vec<Reminder>,
        options: RunOptions,
    ) -> Result<Run> {
        self.admit_execution(session, None, options, false, None, None, None, reminders)
    }
    pub fn start_reminders_if_active(
        &self,
        session: &str,
        reminders: Vec<Reminder>,
        options: RunOptions,
    ) -> Result<Run> {
        self.admit_execution(session, None, options, true, None, None, None, reminders)
    }
    pub fn start_with_reminders(
        &self,
        session: &str,
        text: impl Into<String>,
        reminders: Vec<Reminder>,
        options: RunOptions,
    ) -> Result<Run> {
        self.admit_execution(
            session,
            Some(text.into()),
            options,
            false,
            None,
            None,
            None,
            reminders,
        )
    }
    pub fn steer_reminder(&self, session: &str, reminder: Reminder) -> Result<QueuedInput> {
        let _admission = self.state.admission_guard(session);
        ensure!(
            !self.state.stopping.load(Ordering::Relaxed),
            "Harness is shutting down"
        );
        ensure!(
            reminder.text.len() <= self.config.max_request_bytes,
            "Input exceeds request size limit"
        );
        self.store
            .queue_reminder(session, InputKind::Steer, reminder)
    }
    pub(super) fn admit_reminders_tx(
        tx: &rusqlite::Connection,
        session: &str,
        reminders: &[Reminder],
    ) -> Result<()> {
        for reminder in reminders {
            let input = QueuedInput {
                id: id(),
                session_id: session.into(),
                kind: InputKind::Followup,
                text: reminder.text.clone(),
                state: "pending".into(),
                origin: Some(crate::MessageOrigin::Generated),
            };
            Store::queue_tx(tx, &input)?;
            Store::write_reminder_metadata_tx(
                tx,
                session,
                &input.id,
                &ReminderMetadata::new("root", vec![reminder.clone()]),
            )?;
            tx.execute(
                "UPDATE inputs SET state='reminder_pending' WHERE id=?1",
                [&input.id],
            )?;
        }
        Ok(())
    }
    /// Observe registry republishes without declaring a frozen or cold-tier change.
    pub(super) fn run_tool_snapshot(
        &self,
        session: &str,
        options: &RunOptions,
    ) -> Result<ToolRegistry> {
        let tools = self.selected_tools(session, options)?;
        if self
            .context_state()
            .observe_tool_generation(session, tools.generation())
            && let Some(observer) = &self.config.observation
        {
            let run_id = self
                .state
                .active
                .lock()
                .unwrap()
                .get(session)
                .map(|active| active.run_id.clone())
                .unwrap_or_default();
            observer.emit(
                ObservationScope { session_id: session.into(), run_id, ..Default::default() },
                "context_change",
                serde_json::json!({"reason":"tools_reloaded","prefix_changed":false,"generation":tools.generation()}),
                None,
            );
        }
        Ok(tools)
    }
    pub(super) fn declare_context_change(&self, session: &str, reason: &str) {
        self.state.cache.declare(Some(session), reason);
    }
}
