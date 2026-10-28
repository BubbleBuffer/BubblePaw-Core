//! Question routing and correlated direct-child sends.
use super::*;

impl AgentTools {
    pub fn set_questions(self: &Arc<Self>, questions: Arc<QuestionTools>) -> Result<()> {
        questions.set_agents(Arc::downgrade(self))?;
        self.registry
            .questions
            .set(questions)
            .map_err(|_| anyhow!("question tools are already configured"))
    }
    pub(crate) fn manages_child_question(&self, child: &str) -> Result<bool> {
        let retained = self
            .registry
            .state
            .lock()
            .map_err(|_| anyhow!("agent registry poisoned"))?
            .children
            .values()
            .any(|entry| entry.session_id == child);
        Ok(retained || self.store.attachment(child, BINDING_ID)?.is_some())
    }
    pub(crate) fn child_question_harness(&self, parent: &str, child: &str) -> Result<Arc<Harness>> {
        Ok(self.child(parent, child)?.harness.clone())
    }
    pub(crate) fn continue_child_question(
        &self,
        parent: &str,
        session: &str,
        answer: &crate::question::QuestionAnswer,
    ) -> Result<()> {
        let child = self.child(parent, session)?;
        if answer.live {
            return touch(&child);
        }
        let queued = answer_has_pending_input(&self.store, session, answer)?;
        if queued || crate::question::answer_is_undelivered(&self.store, session, answer)? {
            let approval = child
                .turn
                .lock()
                .map_err(|_| anyhow!("agent result poisoned"))?
                .approval
                .clone();
            let reminders = if queued {
                Vec::new()
            } else {
                vec![answer.continuation_reminder()]
            };
            self.start_wake_child_input(child, reminders, approval, queued)?;
        }
        Ok(())
    }
    /// Restore the ordinary child-parent wake binding before publishing a
    /// persisted grandchild question. Root parents use the host fallback.
    pub fn recover_questions(&self) -> Result<usize> {
        let questions = self
            .registry
            .questions
            .get()
            .ok_or_else(|| anyhow!("question routing is unavailable"))?;
        for session in self.store.list_sessions()? {
            if self
                .registry
                .scope
                .as_ref()
                .is_some_and(|scope| !scope.contains(&self.store, &session.id))
            {
                continue;
            }
            if self.store.active_session_metadata(&session.id)?.is_none() {
                continue;
            }
            if questions.pending(&session.id)?.is_none() {
                continue;
            }
            let Some(parent) = session.parent_session_id else {
                continue;
            };
            let parent_session = self.store.session(&parent)?;
            if let Some(owner) = parent_session.parent_session_id {
                match self.child(&owner, &parent) {
                    Ok(child) => self.register_child_wake(&child),
                    Err(error) => {
                        tracing::warn!(session = %parent, %error, "pending question parent could not be reattached; question remains durable")
                    }
                }
            }
        }
        questions.recover_pending_scoped(
            &self
                .registry
                .scope
                .clone()
                .unwrap_or_else(crate::session_scope::SessionScope::all),
        )
    }
    pub(super) fn question_for_send(&self, session: &str, input: &Value) -> Result<Option<Value>> {
        let current_question = self.store.host_state(session, "question")?;
        if let Some(question_id) = input.get("question_id") {
            let question_id = question_id
                .as_str()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| anyhow!("question_id must be a nonempty string"))?;
            let question = current_question
                .as_ref()
                .filter(|question| question["id"].as_str() == Some(question_id))
                .ok_or_else(|| anyhow!("pending question id does not match"))?;
            ensure!(
                question["status"] != "answered"
                    || crate::question::answered_by(question) != "user",
                "question was already answered by the user"
            );
            ensure!(
                question["status"] == "pending",
                "question is no longer pending"
            );
        } else if let Some(question) = &current_question
            && question["status"] == "answered"
            && crate::question::answered_by(question) == "user"
        {
            let answer = crate::question::QuestionAnswer {
                question: question.clone(),
                live: false,
            };
            ensure!(
                !crate::question::answer_is_undelivered(&self.store, session, &answer)?,
                "question was already answered by the user"
            );
        }
        Ok(current_question)
    }
}
