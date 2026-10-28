//! Host-owned questions. The durable record survives loss of the volatile JS waiter.
use crate::wake::WakeCoordinator;
use anyhow::{Result, anyhow, ensure};
use async_trait::async_trait;
use bubblepaw_core::{
    Approval, ClientInfo, Harness, MessageOrigin, RunOptions, Store, Tool, ToolContext,
    ToolDefinition, ToolRegistry, ToolResult,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock, Weak},
};
use tokio::sync::{Notify, oneshot};

#[path = "question_child.rs"]
mod child;
pub use child::ChildQuestionAnswerError;
use child::validate_user_answer;

#[path = "question_projection.rs"]
mod projection;

const KEY: &str = "question";
const MAX_TEXT: usize = 16 * 1024;

/// Older saved answers were supplied by the parent.
pub fn answered_by(question: &Value) -> &str {
    question["answered_by"].as_str().unwrap_or("parent")
}

/// `live` means the current tool invocation received the answer. Otherwise the
/// host must admit a fresh turn containing `continuation_text()`; replaying the
/// interrupted JS cell could repeat unrelated external effects.
#[derive(Debug)]
pub struct QuestionAnswer {
    pub question: Value,
    pub live: bool,
}
impl QuestionAnswer {
    pub fn continuation_text(&self) -> String {
        format!(
            "[host notice]\n[question answer:{}]\nThe pending question was answered after its original tool execution ended. Continue from this answer; do not replay the interrupted JS cell.\nQuestion: {}\nAnswer: {}",
            self.question["id"].as_str().unwrap_or_default(),
            self.question["text"].as_str().unwrap_or_default(),
            self.question["answer"].as_str().unwrap_or_default()
        )
    }
}

pub struct QuestionTools {
    store: Store,
    wake: Arc<WakeCoordinator>,
    interactive: bool,
    waiters: Mutex<HashMap<String, (String, oneshot::Sender<Value>)>>,
    agents: OnceLock<Weak<crate::agents::AgentTools>>,
    child_answer_harness: OnceLock<Weak<Harness>>,
    pub(crate) changed: Notify,
}
impl QuestionTools {
    pub fn new(store: Store, wake: Arc<WakeCoordinator>, interactive: bool) -> Self {
        Self {
            store,
            wake,
            interactive,
            waiters: Mutex::default(),
            agents: OnceLock::new(),
            child_answer_harness: OnceLock::new(),
            changed: Notify::new(),
        }
    }
    pub fn register(self: &Arc<Self>, registry: &mut ToolRegistry) -> Result<()> {
        registry.register_host(Arc::new(QuestionTool(self.clone())))
    }
    pub fn pending(&self, session: &str) -> Result<Option<Value>> {
        pending_question(&self.store, session)
    }
    pub(crate) fn unconsumed_answer(
        &self,
        harness: &Harness,
        session: &str,
    ) -> Result<Option<QuestionAnswer>> {
        let Some(value) = self.store.host_state(session, KEY)? else {
            return Ok(None);
        };
        if value["status"] != "answered" {
            return Ok(None);
        }
        let answer = if answered_by(&value) == "user" {
            QuestionAnswer {
                live: self.answered_is_live(session, &value)?,
                question: value,
            }
        } else {
            self.answer(
                harness,
                session,
                value["id"].as_str().unwrap_or_default(),
                value["answer"].as_str().unwrap_or_default(),
            )?
        };
        if !answer.live && answer_is_undelivered(&self.store, session, &answer)? {
            Ok(Some(answer))
        } else {
            Ok(None)
        }
    }
    fn unrecorded_answer(&self, session: &str) -> Result<Option<QuestionAnswer>> {
        let Some(question) = self.store.host_state(session, KEY)? else {
            return Ok(None);
        };
        if question["status"] != "answered" {
            return Ok(None);
        }
        let answer = QuestionAnswer {
            question,
            live: false,
        };
        if answer_is_undelivered(&self.store, session, &answer)? {
            Ok(Some(answer))
        } else {
            Ok(None)
        }
    }
    /// Reattach delivery of unanswered child questions, without replaying code.
    pub fn recover_pending(&self) -> Result<usize> {
        self.recover_pending_scoped(&crate::session_scope::SessionScope::all())
    }

    pub fn recover_pending_scoped(
        &self,
        scope: &crate::session_scope::SessionScope,
    ) -> Result<usize> {
        let mut count = 0;
        for session in self.store.list_sessions()? {
            if !scope.contains(&self.store, &session.id) {
                continue;
            }
            if self.store.active_session_metadata(&session.id)?.is_none() {
                continue;
            }
            let _waiters = self
                .waiters
                .lock()
                .map_err(|_| anyhow!("question waiters poisoned"))?;
            if let Some(question) = self.pending(&session.id)? {
                self.publish_question(&session.id, &question);
                count += 1;
            } else if let Some(question) = self.store.host_state(&session.id, KEY)?
                && question["status"] == "answered"
                && answered_by(&question) == "user"
            {
                self.publish_user_answer_if_undelivered(&session.id, &question)?;
            }
        }
        Ok(count)
    }
    pub fn answer(
        &self,
        harness: &Harness,
        session: &str,
        question_id: &str,
        text: &str,
    ) -> Result<QuestionAnswer> {
        self.answer_inner(harness, session, question_id, text, false)
    }
    fn answer_inner(
        &self,
        harness: &Harness,
        session: &str,
        question_id: &str,
        text: &str,
        as_user: bool,
    ) -> Result<QuestionAnswer> {
        if !as_user {
            ensure!(!text.trim().is_empty(), "answer must not be empty");
            ensure!(text.len() <= MAX_TEXT, "answer exceeds 16 KiB");
        }
        let mut waiters = self
            .waiters
            .lock()
            .map_err(|_| anyhow!("question waiters poisoned"))?;
        let current = self
            .store
            .host_state(session, KEY)?
            .ok_or(ChildQuestionAnswerError::NotFound)?;
        if current["id"].as_str() != Some(question_id) {
            return Err(ChildQuestionAnswerError::NotFound.into());
        }
        let has_waiter = waiters
            .get(session)
            .is_some_and(|(id, sender)| id == question_id && !sender.is_closed());
        if !as_user && current["status"] == "answered" {
            ensure!(
                answered_by(&current) != "user",
                "question was already answered by the user"
            );
            ensure!(
                current["answer"].as_str() == Some(text),
                "question was already answered differently"
            );
            if !has_waiter {
                let live = self.answered_is_live(session, &current)?;
                return Ok(QuestionAnswer {
                    question: current,
                    live,
                });
            }
        }
        if !as_user && current["status"] != "pending" && current["status"] != "answered" {
            return Err(ChildQuestionAnswerError::Conflict.into());
        }
        let run = current["run_id"].as_str().unwrap_or_default();
        let mut question = if !as_user && current["status"] == "answered" {
            current.clone()
        } else {
            self.update_question(session, run, "child_question_answer", |value| {
                if as_user {
                    validate_user_answer(value.as_ref(), question_id, text)?;
                }
                let mut value = value.ok_or(ChildQuestionAnswerError::NotFound)?;
                if value["id"].as_str() != Some(question_id) {
                    return Err(ChildQuestionAnswerError::NotFound.into());
                }
                if value["status"] != "pending" {
                    return Err(ChildQuestionAnswerError::Conflict.into());
                }
                if text.trim().is_empty() || text.len() > MAX_TEXT {
                    return Err(ChildQuestionAnswerError::Invalid.into());
                }
                value["status"] = json!("answered");
                value["answer"] = json!(text);
                value["answered_by"] = json!(if as_user { "user" } else { "parent" });
                value["delivery"] = json!(if has_waiter { "live" } else { "continuation" });
                Ok(value)
            })?
        };
        // Replace the parent's waiting notice while the question mutex still
        // excludes publication by ask. Both parent and child clients can watch
        // the existing durable host-state event shape.
        if as_user {
            self.publish_user_answer(session, &question)?;
        } else {
            self.publish_answer_projection(session, &question)?;
        }
        if has_waiter {
            let answer = QuestionAnswer {
                question: question.clone(),
                live: true,
            };
            if answer_is_undelivered(&self.store, session, &answer)?
                && let Err(error) = harness.steer_reminder(session, answer.input_reminder())
            {
                self.store.update_host_state(session, run, KEY, |value| {
                    let mut value = value.ok_or_else(|| anyhow!("question disappeared"))?;
                    value["delivery"] = json!("continuation");
                    Ok(value)
                })?;
                return Err(error);
            }
        }
        let live = waiters
            .remove(session)
            .is_some_and(|(id, sender)| id == question_id && sender.send(question.clone()).is_ok());
        if has_waiter && !live {
            question = self.store.update_host_state(session, run, KEY, |value| {
                let mut value = value.ok_or_else(|| anyhow!("question disappeared"))?;
                ensure!(
                    value["id"].as_str() == Some(question_id),
                    "question changed during answer delivery"
                );
                value["delivery"] = json!("continuation");
                Ok(value)
            })?;
        }
        if !as_user {
            self.remove_notice(session, &question);
        }
        drop(waiters);
        Ok(QuestionAnswer { question, live })
    }
    fn answered_is_live(&self, session: &str, question: &Value) -> Result<bool> {
        Ok(question["delivery"] == "live"
            && self.store.runs(session)?.iter().any(|run| {
                Some(run.id.as_str()) == question["run_id"].as_str()
                    && matches!(run.status.as_str(), "running" | "completed")
            }))
    }
    pub fn cancel(&self, session: &str) -> Result<bool> {
        let mut waiters = self
            .waiters
            .lock()
            .map_err(|_| anyhow!("question waiters poisoned"))?;
        let Some(current) = self.pending(session)? else {
            return Ok(false);
        };
        let run = current["run_id"].as_str().unwrap_or_default();
        let id = current["id"].clone();
        let cancelled = self.update_question(session, run, "child_question", |value| {
            let mut value = value.ok_or_else(|| anyhow!("question disappeared"))?;
            ensure!(
                value["id"] == id && value["status"] == "pending",
                "question is no longer pending"
            );
            value["status"] = json!("cancelled");
            Ok(value)
        })?;
        if let Some((_, sender)) = waiters.remove(session) {
            let _ = sender.send(cancelled.clone());
        }
        drop(waiters);
        self.remove_notice(session, &cancelled);
        Ok(true)
    }
    fn remove_notice(&self, session: &str, question: &Value) {
        if let Some(parent) = question["parent_session_id"].as_str() {
            self.wake.remove(
                parent,
                &notice_key(session, question["id"].as_str().unwrap_or_default()),
            );
        }
    }
    fn publish_question(&self, session: &str, question: &Value) {
        if let Some(parent) = question["parent_session_id"].as_str() {
            let id = question["id"].as_str().unwrap_or_default();
            let text = format!(
                "Child {} is waiting_for_answer. Question: {}\nOptions: {}\nAnswer using tools.agent_send({{sessionId:{},question_id:{},text:<answer>}}).",
                session,
                question["text"].as_str().unwrap_or_default(),
                question["options"],
                json!(session),
                json!(id)
            );
            let reminder =
                bubblepaw_core::Reminder::new(format!("question_waiting:{session}:{id}"), text)
                    .with_delivery(
                        "question_waiting",
                        serde_json::to_string(&[session, id]).expect("question identity is JSON"),
                    );
            self.wake
                .publish_reminder_required(parent, notice_key(session, id), reminder);
        }
    }
    async fn ask(&self, context: ToolContext, input: Value) -> Result<ToolResult> {
        ensure!(!context.cancellation.is_cancelled(), "cancelled");
        let object = input
            .as_object()
            .ok_or_else(|| anyhow!("question expects an object"))?;
        ensure!(
            object
                .keys()
                .all(|key| matches!(key.as_str(), "text" | "options")),
            "question accepts only text and options"
        );
        let text = input["text"]
            .as_str()
            .ok_or_else(|| anyhow!("text must be a string"))?;
        ensure!(
            !text.trim().is_empty() && text.len() <= MAX_TEXT,
            "question text must be nonempty and at most 16 KiB"
        );
        if let Some(options) = input.get("options") {
            let options = options
                .as_array()
                .ok_or_else(|| anyhow!("options must be an array of strings"))?;
            ensure!(
                !options.is_empty()
                    && options.len() <= 16
                    && options.iter().all(|option| option
                        .as_str()
                        .is_some_and(|s| !s.trim().is_empty() && s.len() <= 1024)),
                "options must contain 1 to 16 nonempty strings of at most 1024 bytes"
            );
        }
        let parent = self.store.session(&context.session_id)?.parent_session_id;
        if parent.is_none() && !self.interactive {
            return Ok(ToolResult::text(json!({"status":"no_user_available","reason":"No user is available in this unattended host."}).to_string()));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let question = json!({"id":id,"status":"pending","text":text,"options":input.get("options"),"run_id":context.run_id,"parent_session_id":parent,"asked_at":chrono::Utc::now().to_rfc3339()});
        let receiver = {
            let mut waiters = self
                .waiters
                .lock()
                .map_err(|_| anyhow!("question waiters poisoned"))?;
            // The answer and its continuation admission are separate durable
            // boundaries. Preserve an answer whose admission has not succeeded.
            // Read history before entering the store's update transaction.
            if let Some(previous) = self.store.host_state(&context.session_id, KEY)?
                && previous["status"] == "answered"
                && !self.answered_is_live(&context.session_id, &previous)?
            {
                let answer = QuestionAnswer {
                    question: previous,
                    live: false,
                };
                ensure!(
                    !answer_is_undelivered(&self.store, &context.session_id, &answer)?,
                    "the previous answered question still awaits continuation admission"
                );
            }
            self.update_question(
                &context.session_id,
                &context.run_id,
                "child_question",
                |value| {
                    ensure!(
                        !value.is_some_and(|value| value["status"] == "pending"),
                        "session already has a pending question"
                    );
                    Ok(question.clone())
                },
            )?;
            let (sender, receiver) = oneshot::channel();
            waiters.insert(context.session_id.clone(), (id.clone(), sender));
            self.publish_question(&context.session_id, &question);
            receiver
        };
        self.changed.notify_waiters();
        let answer = tokio::select! {
            answer = receiver => answer.map_err(|_| anyhow!("question waiter closed"))?,
            _ = context.cancellation.cancelled() => {
                self.cancel(&context.session_id)?;
                json!({"id":id,"status":"cancelled"})
            }
        };
        Ok(ToolResult::text(answer.to_string()))
    }
}

pub(crate) fn pending_question(store: &Store, session: &str) -> Result<Option<Value>> {
    Ok(store
        .host_state(session, KEY)?
        .filter(|value| value["status"] == "pending"))
}
fn notice_key(session: &str, id: &str) -> String {
    format!("question:{session}:{id}")
}

#[path = "question_reminders.rs"]
mod reminders;
pub use reminders::continue_answer;
pub(crate) use reminders::{answer_has_pending_input, answer_is_undelivered};

struct QuestionTool(Arc<QuestionTools>);
#[async_trait]
impl Tool for QuestionTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition { name:"question".into(), description:"Ask a clarifying question and wait for an answer. Child questions go to their direct parent, who answers through agent_send; root questions go to the client. Unattended root hosts return no_user_available immediately. Options are suggestions; a free text answer is accepted.".into(), input_schema:json!({"type":"object","properties":{"text":{"type":"string","minLength":1,"maxLength":16384},"options":{"type":"array","minItems":1,"maxItems":16,"items":{"type":"string","minLength":1,"maxLength":1024}}},"required":["text"],"additionalProperties":false}), read_only:true, approval:Approval::None }
    }
    async fn invoke(&self, context: ToolContext, input: Value) -> Result<ToolResult> {
        self.0.ask(context, input).await
    }
    fn take_reminders(&self, session: &str) -> Vec<bubblepaw_core::Reminder> {
        self.0.take_reminders(session)
    }
    fn cancel_session(&self, session: &str) -> usize {
        usize::from(self.0.cancel(session).unwrap_or(false))
    }
}

#[cfg(test)]
#[path = "question_tests.rs"]
mod tests;
