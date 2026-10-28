//! Parent question projections share their child's existing durable boundary.
use super::*;

fn projection(session: &str, question: &Value) -> Value {
    let mut value = json!({
        "child_session_id":session,
        "id":question["id"],
        "parent_session_id":question["parent_session_id"],
        "status":question["status"],
        "text":question["text"],
        "options":question["options"],
        "asked_at":question["asked_at"],
        "run_id":question["run_id"],
    });
    if question["status"] == "answered" {
        value["answer"] = question["answer"].clone();
        value["answered_by"] = json!(answered_by(question));
    }
    value
}

impl QuestionTools {
    pub(super) fn update_question(
        &self,
        session: &str,
        run: &str,
        projection_key: &str,
        update: impl FnOnce(Option<Value>) -> Result<Value>,
    ) -> Result<Value> {
        let parent = self
            .store
            .active_session_metadata(session)?
            .and_then(|session| session.parent_session_id);
        if let Some(parent) = parent {
            self.store.update_host_state_with_projection(
                session,
                run,
                KEY,
                (&parent, projection_key),
                |previous| {
                    let value = update(previous)?;
                    let projected = projection(session, &value);
                    Ok((value, projected))
                },
            )
        } else {
            self.store.update_host_state(session, run, KEY, update)
        }
    }

    /// Repair a legacy or interrupted cross-database answer projection. Normal
    /// same-database answers already committed it with the child and add no write.
    pub(super) fn publish_answer_projection(&self, session: &str, question: &Value) -> Result<()> {
        if let Some(parent) = question["parent_session_id"].as_str() {
            let value = projection(session, question);
            if self
                .store
                .host_state(parent, "child_question_answer")?
                .as_ref()
                != Some(&value)
            {
                self.store
                    .update_host_state(parent, "", "child_question_answer", |_| Ok(value))?;
            }
        }
        Ok(())
    }
}
