//! Waiting on children, persisted run controls and bounded replies.
use super::*;

pub(super) fn touch(child: &Child) -> Result<()> {
    let mut turn = child
        .turn
        .lock()
        .map_err(|_| anyhow!("agent result poisoned"))?;
    turn.last_interaction = Instant::now();
    turn.last_reminder = None;
    Ok(())
}
pub(super) struct WaitInteraction(Arc<Child>);
impl WaitInteraction {
    pub(super) fn new(child: Arc<Child>) -> Result<Self> {
        {
            let mut turn = child
                .turn
                .lock()
                .map_err(|_| anyhow!("agent result poisoned"))?;
            turn.active_waits += 1;
            turn.last_interaction = Instant::now();
            turn.last_reminder = None;
        }
        Ok(Self(child))
    }
}
impl Drop for WaitInteraction {
    fn drop(&mut self) {
        if let Ok(mut turn) = self.0.turn.lock() {
            turn.active_waits = turn.active_waits.saturating_sub(1);
            turn.last_interaction = Instant::now();
            turn.last_reminder = None;
        }
    }
}
pub(super) fn load_controls(store: &Store, session: &str, run: &str) -> Result<Controls> {
    store
        .attachment(session, &format!("{CONTROLS_PREFIX}{run}"))?
        .map(|(metadata, _)| serde_json::from_str(&metadata))
        .transpose()
        .map(|v| v.unwrap_or_default())
        .map_err(Into::into)
}
pub(super) fn save_controls(
    store: &Store,
    session: &str,
    run: &str,
    controls: &Controls,
) -> Result<()> {
    store
        .store_attachment(
            session,
            &format!("{CONTROLS_PREFIX}{run}"),
            &serde_json::to_string(controls)?,
            b"",
        )
        .map(|_| ())
}
pub(super) fn capture_partial(
    event: HarnessEvent,
    session: &str,
    run: &str,
    partial: &mut String,
    bytes: &mut u64,
    gap: &mut bool,
) {
    if event.session_id != session || event.run_id != run {
        return;
    }
    match event.kind {
        EventKind::ModelRequestStarted {} => {
            partial.clear();
            *bytes = 0;
            *gap = false;
        }
        EventKind::TextDelta { text } => {
            *bytes = bytes.saturating_add(text.len() as u64);
            let mut end = text
                .len()
                .min(MAX_REPLY_BYTES.saturating_sub(partial.len()));
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            partial.push_str(&text[..end]);
        }
        // The streamed assistant tool round is canonical now; a later provider
        // response starts a fresh partial answer.
        EventKind::ToolStarted { .. } => {
            partial.clear();
            *bytes = 0;
        }
        _ => {}
    }
}
pub(super) fn cancelled_run(store: &Store, session: &str, run: Option<&str>) -> Result<bool> {
    if let Some(run) = run {
        if store
            .attachment(session, &format!("{CANCEL_PREFIX}{run}"))?
            .is_some()
        {
            return Ok(true);
        }
        // A new run explicitly supersedes legacy whole-session cancellation.
        if store
            .attachment(session, &format!("{CONTROLS_PREFIX}{run}"))?
            .is_some()
        {
            return Ok(false);
        }
    }
    Ok(store.attachment(session, CANCEL_ID)?.is_some())
}
pub(super) fn add_json(reply: &mut Value, controls: &Controls) {
    if controls.output_schema.is_none() {
        return;
    }
    match serde_json::from_str::<Value>(reply["text"].as_str().unwrap_or("")) {
        Ok(value) => {
            reply["json"] = value;
            // Reserve the small status/read-receipt fields added by snapshots.
            if reply.to_string().len().saturating_add(128) > MAX_REPLY_BYTES {
                reply
                    .as_object_mut()
                    .expect("child reply object")
                    .remove("json");
                reply["status"] = json!("failed");
                reply["error"] = json!(
                    "Validated JSON exceeds child reply size limit of 60 KiB; full text remains in child history"
                );
            }
        }
        Err(error) => {
            reply["status"] = json!("failed");
            reply["error"] = json!(format!(
                "Validated child output could not be parsed as JSON: {error}"
            ));
        }
    }
}
pub(super) async fn await_child(child: &Child, questions: Option<&QuestionTools>) -> Result<()> {
    loop {
        let changed = child.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        let mut question_changed =
            questions.map(|questions| Box::pin(questions.changed.notified()));
        if let Some(changed) = &mut question_changed {
            changed.as_mut().enable();
        }
        if pending_question(&child.store, &child.session_id)?.is_some() {
            return Ok(());
        }
        if child
            .turn
            .lock()
            .map_err(|_| anyhow!("agent result poisoned"))?
            .result
            .is_some()
        {
            return Ok(());
        }
        tokio::select! {
            _ = changed => {},
            _ = async { if let Some(changed) = question_changed { changed.await } else { std::future::pending::<()>().await } } => {},
        }
    }
}
pub(super) async fn await_cancelled_turn(child: &Child, run_id: &Option<String>) -> Result<()> {
    loop {
        let changed = child.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        {
            let turn = child
                .turn
                .lock()
                .map_err(|_| anyhow!("agent result poisoned"))?;
            if &turn.run_id != run_id || turn.result.is_some() {
                return Ok(());
            }
        }
        changed.await;
    }
}
/// Store run records deliberately omit text; restore the last run's result
/// from canonical history using the same time boundaries as core hydration.
pub(super) fn restore_text(store: &Store, run: &mut Run) -> Result<()> {
    if run.text.is_some() {
        return Ok(());
    }
    let start = chrono::DateTime::parse_from_rfc3339(&run.started_at)?;
    let end = chrono::DateTime::parse_from_rfc3339(
        run.ended_at
            .as_deref()
            .ok_or_else(|| anyhow!("Completed run has no end time"))?,
    )?;
    for message in store.messages(&run.session_id)?.into_iter().rev() {
        let created = chrono::DateTime::parse_from_rfc3339(&message.created_at)?;
        if created < start {
            break;
        }
        if created <= end
            && (message.role == Role::Assistant
                || (run.model == bubblepaw_core::SCRIPT_MODEL && message.role == Role::Tool))
        {
            run.text = Some(message.content);
            break;
        }
    }
    Ok(())
}
pub(super) fn terminal_reply(handle: &str, run: &Run) -> Value {
    let status = match run.status.as_str() {
        "completed" => "idle",
        "cancelled" => "cancelled",
        _ => "failed",
    };
    let mut value = json!({"handle":handle,"sessionId":run.session_id,"status":status,"text":run.text.as_deref().unwrap_or("")});
    if let Some(error) = &run.error {
        value["error"] = json!(error);
    }
    if let Some(usage) = &run.usage {
        value["usage"] = json!(usage);
    }
    value
}
pub(super) async fn close_environments(repl: &CodeMode, session: &str) -> Result<()> {
    let started = Instant::now();
    let mut last_error = String::new();
    loop {
        // Re-read inventory so a concurrent child continuation cannot introduce
        // an unclosed environment after a one-time snapshot.
        let inventory = repl.status(session);
        let environments = inventory["environments"]
            .as_array()
            .ok_or_else(|| anyhow!("Invalid child environment inventory"))?;
        if environments.is_empty() {
            return Ok(());
        }
        ensure!(
            started.elapsed() < Duration::from_secs(5),
            "Timed out closing child environments after 5 seconds{}",
            if last_error.is_empty() {
                String::new()
            } else {
                format!(": {last_error}")
            }
        );
        for environment in environments
            .iter()
            .filter(|environment| environment["status"] != "closing")
        {
            let name = environment["name"]
                .as_str()
                .ok_or_else(|| anyhow!("Child environment has no name"))?;
            if let Err(error) = repl.control(
                session,
                json!({"action":"close","environment":name,"cancel":true}),
            ) {
                last_error = error.to_string().chars().take(512).collect();
            }
        }
        // status reaps closed threads. Cancellation wakes explicit waits; the
        // deadline above reports failure instead of waiting indefinitely.
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

pub(super) fn bounded_reply(mut value: Value) -> Value {
    if value.get("json").is_some() && value.to_string().len() > MAX_REPLY_BYTES {
        value
            .as_object_mut()
            .expect("child reply object")
            .remove("json");
        value["status"] = json!("failed");
        value["error"] = json!(
            "Validated JSON exceeds child reply size limit of 60 KiB; full text remains in child history"
        );
    }
    let original_text = value["textBytes"]
        .as_u64()
        .unwrap_or_else(|| value["text"].as_str().unwrap_or("").len() as u64);
    let original_error = value["error"].as_str().map(str::len);
    if value.to_string().len() <= MAX_REPLY_BYTES {
        return value;
    }
    value["resultTruncated"] = json!(true);
    value["textBytes"] = json!(original_text);
    if let Some(bytes) = original_error {
        value["errorBytes"] = json!(bytes);
    }
    while value.to_string().len() > MAX_REPLY_BYTES {
        let key = if value["text"].as_str().unwrap_or("").len()
            >= value["error"].as_str().unwrap_or("").len()
        {
            "text"
        } else {
            "error"
        };
        let text = value[key].as_str().unwrap_or("");
        let mut end = text.len() / 2;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        value[key] = json!(&text[..end]);
    }
    value
}
