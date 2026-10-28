//! Host-only direct-child answers and their typed request errors.
use super::*;

/// Errors from the host-only direct-child answer command.
#[derive(Debug)]
pub enum ChildQuestionAnswerError {
    NotFound,
    Conflict,
    Invalid,
    Internal(anyhow::Error),
}
impl std::fmt::Display for ChildQuestionAnswerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("pending child question not found"),
            Self::Conflict => formatter.write_str("question was already answered or cancelled"),
            Self::Invalid => formatter.write_str("answer must be nonempty and at most 16 KiB"),
            Self::Internal(error) => std::fmt::Display::fmt(error, formatter),
        }
    }
}
impl std::error::Error for ChildQuestionAnswerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Internal(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}
impl From<anyhow::Error> for ChildQuestionAnswerError {
    fn from(error: anyhow::Error) -> Self {
        match error.downcast::<Self>() {
            Ok(error) => error,
            Err(error) => Self::Internal(error),
        }
    }
}

impl QuestionTools {
    pub(crate) fn set_agents(&self, agents: Weak<crate::agents::AgentTools>) -> Result<()> {
        self.agents
            .set(agents)
            .map_err(|_| anyhow!("child question routing is already configured"))
    }
    /// Fallback for hosts that run child sessions directly, without AgentTools.
    /// Saved agent bindings always retain their original agent harness instead.
    pub fn set_child_answer_harness(&self, harness: Arc<Harness>) -> Result<()> {
        let weak = Arc::downgrade(&harness);
        if let Err(weak) = self.child_answer_harness.set(weak) {
            ensure!(
                self.child_answer_harness
                    .get()
                    .is_some_and(|current| Weak::ptr_eq(current, &weak)),
                "child answer harness is already configured"
            );
        }
        Ok(())
    }
    /// Answer one pending question belonging to a direct child. This command is
    /// host-only: it is not registered as a model tool. Successful return means
    /// the live tool received the answer or its safe continuation was admitted.
    pub async fn answer_child_as_user(
        &self,
        parent: &str,
        child: &str,
        question_id: &str,
        text: &str,
    ) -> std::result::Result<(), ChildQuestionAnswerError> {
        self.store
            .active_session_metadata(child)?
            .filter(|session| session.parent_session_id.as_deref() == Some(parent))
            .ok_or(ChildQuestionAnswerError::NotFound)?;
        // Report request errors independently of whether delivery can currently
        // reattach. The update below repeats these checks under the store lock.
        validate_user_answer(
            self.store.host_state(child, KEY)?.as_ref(),
            question_id,
            text,
        )?;
        // Resolve the host only after the catalog ownership check. A managed
        // child's profile must never fall back to the root host on an error.
        let agents = self
            .agents
            .get()
            .map(|agents| {
                agents
                    .upgrade()
                    .ok_or_else(|| anyhow!("child question routing host was released"))
            })
            .transpose()?;
        let agents = match agents {
            Some(agents) if agents.manages_child_question(child)? => Some(agents),
            _ => None,
        };
        let harness = match &agents {
            Some(agents) => agents.child_question_harness(parent, child)?,
            None => self
                .child_answer_harness
                .get()
                .and_then(Weak::upgrade)
                .ok_or_else(|| anyhow!("child question delivery host is unavailable"))?,
        };
        let answer = self.answer_inner(&harness, child, question_id, text, true)?;
        if let Some(agents) = agents {
            agents.continue_child_question(parent, child, &answer)?;
        } else {
            continue_answer(&harness, &self.store, child, &answer, None)?;
        }
        Ok(())
    }
}

pub(super) fn validate_user_answer(
    value: Option<&Value>,
    question_id: &str,
    text: &str,
) -> std::result::Result<(), ChildQuestionAnswerError> {
    let value = value
        .filter(|value| value["id"].as_str() == Some(question_id))
        .ok_or(ChildQuestionAnswerError::NotFound)?;
    if value["status"] != "pending" {
        return Err(ChildQuestionAnswerError::Conflict);
    }
    if text.trim().is_empty() || text.len() > MAX_TEXT {
        return Err(ChildQuestionAnswerError::Invalid);
    }
    Ok(())
}
