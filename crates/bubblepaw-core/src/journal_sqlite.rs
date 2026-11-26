//! The journal over the SQLite store and the event log: what the run loop wrote
//! directly before the seam, with the same transactions and events.
use crate::event_log::{ContentRef, Log};
use crate::journal::{RunFinish, SessionJournal};
use crate::journal_admission::RunAdmission;
use crate::run_journal::{Consumed, InputScope, RunJournal};
use crate::{
    ClientInfo, EventKind, Harness, InputKind, Message, OutputRecord, QueuedInput, Run, Store,
    ToolCall, ToolResult,
};
use anyhow::Result;
use std::sync::Arc;

pub(crate) struct SqliteJournal {
    store: Store,
    log: Arc<Log>,
    /// Holds the run's request usage until the commits that write it.
    state: Arc<crate::runtime::RuntimeState>,
}

/// The one journal seen from the shell's two sides.
#[derive(Clone)]
pub(crate) struct Journals {
    pub(crate) session: Arc<dyn SessionJournal>,
    pub(crate) run: Arc<dyn RunJournal>,
}

impl Journals {
    pub(crate) fn new(store: Store, log: Arc<Log>) -> Self {
        let journal = Arc::new(SqliteJournal::new(store, log));
        Self {
            session: journal.clone(),
            run: journal,
        }
    }
}

impl SqliteJournal {
    pub(crate) fn new(store: Store, log: Arc<Log>) -> Self {
        let state = store.runtime();
        Self { store, log, state }
    }
}

impl SessionJournal for SqliteJournal {
    fn run_admitted(&self, admission: RunAdmission<'_>) -> Result<()> {
        let RunAdmission {
            run,
            content,
            origin,
            client,
            tool_only,
            passengers,
        } = admission;
        let session_id = run.session_id.as_str();
        self.log
            .commit(&self.store, session_id, &run.id, client, |tx| {
                if let Some(expected) = passengers.expected_wait {
                    Store::ensure_quota_resume_tx(tx, expected)?;
                }
                Store::bind_execution_mode_tx(tx, session_id, passengers.execution_mode)?;
                if let Some(surface) = passengers.surface {
                    Store::bind_chat_tool_surface_tx(tx, session_id, surface)?;
                }
                if tool_only {
                    // A tool-only run answers no queued conversational input.
                    Store::start_run_tx(tx, run)?;
                } else {
                    Store::admit_tx(tx, run, content, origin)?;
                }
                Harness::admit_reminders_tx(tx, session_id, passengers.reminders)?;
                Ok((
                    (),
                    vec![(
                        EventKind::RunStarted {
                            model: run.model.clone(),
                        },
                        None,
                    )],
                ))
            })
    }
    fn input_queued(
        &self,
        input: &QueuedInput,
        run_id: Option<&str>,
        client: Option<ClientInfo>,
    ) -> Result<()> {
        let events_text = crate::content_body_text(&input.content).into_owned();
        let events_content = crate::extra_content(&input.content);
        self.log.commit(
            &self.store,
            &input.session_id,
            run_id.unwrap_or_default(),
            client,
            |tx| {
                Store::queue_tx(tx, input)?;
                let events = run_id
                    .map(|_| {
                        let kind = if input.kind == InputKind::Steer {
                            EventKind::SteeringAccepted {
                                text: events_text.clone(),
                                content: events_content.clone(),
                            }
                        } else {
                            EventKind::FollowupQueued {
                                text: events_text.clone(),
                                content: events_content.clone(),
                            }
                        };
                        vec![(
                            kind,
                            Some(ContentRef::Input {
                                input_id: input.id.clone(),
                            }),
                        )]
                    })
                    .unwrap_or_default();
                Ok(((), events))
            },
        )
    }
    fn run_finished(&self, finish: RunFinish<'_>) -> Result<()> {
        let RunFinish {
            run,
            result_message_id,
            final_message,
            origin,
            settles_session,
        } = finish;
        // The run ends here: what it waited to write goes in this commit or nowhere.
        let (usage, request_ids) = self.state.usage.take(&run.id);
        let (usage, request_ids) = (usage.as_slice(), request_ids.as_slice());
        self.log
            .commit(&self.store, &run.session_id, &run.id, origin, |tx| {
                if let Some(message) = final_message {
                    Store::insert_message(tx, message)?;
                    Store::insert_message_request_usage_tx(tx, message, usage, request_ids)?;
                } else {
                    Store::insert_request_usage_tx(tx, usage)?;
                }
                Store::finish_run_tx(tx, run, result_message_id)?;
                let mut events = vec![(
                    EventKind::RunFinished {
                        status: run.status.clone(),
                        text: run.text.clone(),
                        error: run.error.clone(),
                        usage: run.usage.clone(),
                    },
                    Some(ContentRef::Run {
                        run_id: run.id.clone(),
                    }),
                )];
                if settles_session {
                    events.push((EventKind::SessionSettled {}, None));
                }
                Ok(((), events))
            })
    }
    fn run_continued(&self, next: &Run) -> Result<()> {
        self.log
            .commit(&self.store, &next.session_id, &next.id, None, |tx| {
                Store::start_run_tx(tx, next)?;
                Store::consume_tx(tx, &next.session_id, false, true)?;
                Ok((
                    (),
                    vec![(
                        EventKind::RunStarted {
                            model: next.model.clone(),
                        },
                        None,
                    )],
                ))
            })
    }
    fn session_settled(&self, session_id: &str, run_id: &str) {
        self.log.emit(
            &self.store,
            session_id,
            run_id,
            EventKind::SessionSettled {},
            None,
        );
    }
}

impl RunJournal for SqliteJournal {
    fn input_pending(&self, session_id: &str, scope: InputScope) -> Result<bool> {
        match scope {
            InputScope::Any => self.store.has_pending(session_id),
            InputScope::Steering => Ok(self
                .store
                .queued_inputs(session_id)?
                .iter()
                .any(|input| input.state == "pending" && input.kind == InputKind::Steer)),
        }
    }
    fn inputs_consumed(&self, session_id: &str, which: Consumed) -> Result<usize> {
        match which {
            Consumed::Steering => self.store.consume(session_id, true, false),
            Consumed::GeneratedSteering => self.store.consume_generated_steering(session_id),
        }
    }
    fn steering_batch_consumed(&self, session_id: &str) -> Result<Vec<Message>> {
        self.store.consume_steering_messages(session_id)
    }
    fn tool_call_recorded(&self, session_id: &str, call_id: &str) -> Result<bool> {
        self.store.has_tool_call(session_id, call_id)
    }
    fn reply_received(&self, run_id: &str, message: &Message) -> Result<()> {
        let (usage, request_ids) = self.state.usage.waiting(run_id);
        self.log.settle_text(&message.session_id, || {
            self.store
                .append_with_request_usage(message, &usage, &request_ids)
        })?;
        self.state.usage.written(run_id);
        Ok(())
    }
    fn reply_dispatched(
        &self,
        session_id: &str,
        run_id: &str,
        message: &Message,
        call: &ToolCall,
    ) -> Result<String> {
        let (usage, request_ids) = self.state.usage.waiting(run_id);
        let invocation = self
            .log
            .commit_reply(&self.store, session_id, run_id, None, |tx| {
                Store::insert_message(tx, message)?;
                Store::insert_message_request_usage_tx(tx, message, &usage, &request_ids)?;
                let invocation = Store::dispatch_tx(tx, session_id, run_id, call)?;
                Ok((
                    invocation,
                    vec![(
                        EventKind::ToolStarted { call: call.clone() },
                        Some(ContentRef::ToolCall {
                            message_id: message.id.clone(),
                            call_id: call.id.clone(),
                        }),
                    )],
                ))
            })?;
        self.state.usage.written(run_id);
        Ok(invocation)
    }
    fn tool_started(
        &self,
        session_id: &str,
        run_id: &str,
        message_id: &str,
        call: &ToolCall,
        dispatched: bool,
    ) -> Result<Option<String>> {
        self.log
            .commit(&self.store, session_id, run_id, None, |tx| {
                let invocation = if dispatched {
                    Some(Store::dispatch_tx(tx, session_id, run_id, call)?)
                } else {
                    None
                };
                Ok((
                    invocation,
                    vec![(
                        EventKind::ToolStarted { call: call.clone() },
                        Some(ContentRef::ToolCall {
                            message_id: message_id.into(),
                            call_id: call.id.clone(),
                        }),
                    )],
                ))
            })
    }
    fn tool_finished(
        &self,
        run_id: &str,
        message: &Message,
        call: &ToolCall,
        result: ToolResult,
        invocation: Option<&str>,
    ) -> Result<()> {
        self.log
            .commit(&self.store, &message.session_id, run_id, None, |tx| {
                Store::tool_completed_tx(tx, message, invocation, result.pending)?;
                Ok((
                    (),
                    vec![(
                        EventKind::ToolFinished {
                            call_id: call.id.clone(),
                            name: call.name.clone(),
                            result,
                        },
                        Some(ContentRef::ToolResult {
                            message_id: message.id.clone(),
                        }),
                    )],
                ))
            })
    }
    fn nested_dispatched(&self, session_id: &str, run_id: &str, call: &ToolCall) -> Result<String> {
        self.store.dispatch_nested(session_id, run_id, call)
    }
    fn effect_unknown(&self, session_id: &str, invocation: &str) -> Result<()> {
        self.store
            .effect_unknown_for_session(session_id, invocation)
    }
    fn effect_completed(&self, invocation: &str) -> Result<()> {
        self.store.effect_completed(invocation)
    }
    fn effect_completed_for_session(&self, session_id: &str, invocation: &str) -> Result<()> {
        self.store
            .effect_completed_for_session(session_id, invocation)
    }
    fn tool_output(&self, record: &OutputRecord) -> Result<()> {
        self.log.commit(
            &self.store,
            &record.session_id,
            &record.run_id,
            None,
            |tx| {
                Store::append_output_tx(tx, record)?;
                Ok((
                    (),
                    vec![(
                        EventKind::Output {
                            record: record.clone(),
                        },
                        Some(ContentRef::Output {
                            output_id: record.id.clone(),
                        }),
                    )],
                ))
            },
        )
    }
    fn partial_reply_kept(&self, message: &Message) -> Result<()> {
        self.log.settle_text(&message.session_id, || {
            self.store.append_batch(std::slice::from_ref(message))
        })
    }
    fn run_closed(&self, session_id: &str, run_id: &str) -> Result<()> {
        self.store.close_unresolved(session_id, run_id)
    }
}
