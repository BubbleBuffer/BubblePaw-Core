//! Run admission, committed before asynchronous execution.
use super::*;
impl Harness {
    pub(super) fn admit(
        &self,
        session_id: &str,
        text: Option<String>,
        options: RunOptions,
        active_storage_only: bool,
    ) -> Result<Run> {
        self.admit_execution(
            session_id,
            text,
            options,
            active_storage_only,
            None,
            None,
            None,
            Vec::new(),
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) fn admit_execution(
        &self,
        session_id: &str,
        text: Option<String>,
        mut options: RunOptions,
        active_storage_only: bool,
        single_tool: Option<SingleToolRun>,
        cancellation: Option<CancellationToken>,
        expected_quota_wait: Option<&crate::QuotaWait>,
        reminders: Vec<crate::Reminder>,
    ) -> Result<Run> {
        ensure!(
            options.max_rounds.or(self.config.max_rounds) != Some(0),
            "max_rounds must be positive"
        );
        if let Some(limit) = options.max_tool_calls {
            let footer_bytes = format!("\n\n{limit} tool calls left").len();
            ensure!(
                self.config.max_tool_result_bytes >= footer_bytes,
                "max_tool_result_bytes must be at least {footer_bytes} bytes to fit the tool budget footer"
            );
        }
        ensure!(
            options.script.is_none() || options.output_schema.is_none(),
            "output_schema requires a model run and cannot be combined with script"
        );
        if let Some(schema) = &options.output_schema {
            schema.validate()?;
            jsonschema::validator_for(&schema.schema).context("Invalid run output schema")?;
        }
        let mut active = self.state.admission_guard(session_id);
        ensure!(
            !self
                .state
                .compacting
                .lock()
                .unwrap()
                .contains_key(session_id),
            "Session is compacting"
        );
        ensure!(
            !self.state.stopping.load(Ordering::Relaxed),
            "Harness is shutting down"
        );
        if let Some(running) = active.get(session_id).filter(|a| a.finished.is_none()) {
            return Err(SessionBusy {
                session_id: session_id.into(),
                run_id: running.run_id.clone(),
            }
            .into());
        }
        let session = if active_storage_only {
            self.store
                .active_session_metadata(session_id)?
                .ok_or_else(|| anyhow!("Session storage is not active: {session_id}"))?
        } else {
            self.store.session(session_id)?
        };
        ensure!(!session.read_only, "Session is read only");
        ensure!(
            tokio::runtime::Handle::try_current().is_ok(),
            "Starting a run requires a Tokio runtime"
        );
        if let Some(value) = &text {
            ensure!(
                value.len() <= self.config.max_request_bytes,
                "Input exceeds request size limit"
            );
        }
        ensure!(
            reminders
                .iter()
                .all(|reminder| reminder.text.len() <= self.config.max_request_bytes),
            "Input exceeds request size limit"
        );
        let mut run = self.new_run(session_id, &options);
        if single_tool.is_some() {
            run.model = "tool".into();
        }
        let initial_surface = self.initial_chat_surface(session_id, &options)?;
        self.state.log.commit(
            &self.store,
            session_id,
            &run.id,
            options.client.take(),
            |tx| {
                if let Some(expected) = expected_quota_wait {
                    Store::ensure_quota_resume_tx(tx, expected)?;
                }
                Store::bind_execution_mode_tx(tx, session_id, self.config.execution_mode)?;
                if let Some(surface) = &initial_surface {
                    Store::bind_chat_tool_surface_tx(tx, session_id, surface)?;
                }
                if single_tool.is_some() {
                    // A tool-only run answers no queued conversational input.
                    Store::start_run_tx(tx, &run)?;
                } else {
                    Store::admit_tx(tx, &run, text.as_deref(), options.origin)?;
                }
                Self::admit_reminders_tx(tx, session_id, &reminders)?;
                Ok((
                    (),
                    vec![(
                        EventKind::RunStarted {
                            model: run.model.clone(),
                        },
                        None,
                    )],
                ))
            },
        )?;
        let cancellation = cancellation.unwrap_or_default();
        active.insert(
            session_id.into(),
            Active {
                run_id: run.id.clone(),
                cancellation: cancellation.clone(),
                notify: Arc::new(Notify::new()),
                finished: None,
                panicked: None,
                finalizing: false,
                cancelled_by: None,
            },
        );
        // Supervised, so a panicking turn settles this run instead of leaving
        // its waiters blocked forever (runtime_supervision.rs).
        self.spawn_drive(run.clone(), options, cancellation, single_tool);
        Ok(run)
    }
}
/// Same wire size as a previously measured message (request byte admission).
pub(super) fn same_message_wire_size(left: &Message, right: &Message) -> bool {
    left == right
        && left
            .tool_calls
            .iter()
            .zip(&right.tool_calls)
            .all(|(left, right)| same_json_number_bits(&left.arguments, &right.arguments))
        && match (&left.provider_state, &right.provider_state) {
            (Some(left), Some(right)) => same_json_number_bits(left, right),
            (None, None) => true,
            _ => false,
        }
}
// Value equality considers +0.0 and -0.0 equal, although their JSON byte
// lengths differ. Inspect numbers without reserializing unchanged payloads.
fn same_json_number_bits(left: &serde_json::Value, right: &serde_json::Value) -> bool {
    use serde_json::Value;
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => {
            left.as_f64().map(f64::to_bits) == right.as_f64().map(f64::to_bits)
        }
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| same_json_number_bits(left, right))
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, left)| {
                    right
                        .get(key)
                        .is_some_and(|right| same_json_number_bits(left, right))
                })
        }
        _ => true,
    }
}
