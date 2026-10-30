use super::*;
use anyhow::Context;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingChildQuestion {
    pub id: String,
    pub text: String,
    pub options: Vec<String>,
    pub asked_at: Option<String>,
    pub parent_session_id: String,
}
#[derive(Serialize, Deserialize)]
pub struct AnswerResult {
    pub question: Value,
    pub live: bool,
}
pub type PendingResult = Option<PendingChildQuestion>;
pub type AnswerChildResult = AnsweredResult;
/// Metadata-only: absent and archived children remain unopened.
pub fn pending_child(
    ctx: &HostCtx,
    child: &str,
) -> Result<Option<PendingChildQuestion>, HostError> {
    if !ctx
        .store
        .active_session_metadata(child)?
        .is_some_and(|s| s.parent_session_id.is_some())
    {
        return Ok(None);
    }
    ctx.ensure_read_session(child)?;
    let Some(value) = ctx
        .store
        .host_state(child, "question")?
        .filter(|v| v["status"] == "pending")
    else {
        return Ok(None);
    };
    let Some(parent) = value["parent_session_id"].as_str() else {
        return Ok(None);
    };
    Ok(Some(PendingChildQuestion {
        id: value["id"]
            .as_str()
            .context("question id is missing")?
            .into(),
        text: value["text"]
            .as_str()
            .context("question text is missing")?
            .into(),
        options: value["options"]
            .as_array()
            .map(|o| {
                o.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        asked_at: value["asked_at"].as_str().map(str::to_owned),
        parent_session_id: parent.into(),
    }))
}
pub async fn answer_child(
    ctx: &HostCtx,
    input: ChildQuestionParams,
) -> Result<AnswerChildResult, HostError> {
    let questions = required(&ctx.questions, "Question service")?;
    for id in [&input.parent, &input.child] {
        ctx.ensure_session(id).map_err(|error| {
            if error.http_status() == 404 {
                HostError::from(anyhow::Error::new(
                    crate::question::ChildQuestionAnswerError::NotFound,
                ))
            } else {
                error
            }
        })?;
    }
    ctx.input_limits.validate_prompt(&input.text).map_err(|_| {
        HostError::from(anyhow::Error::new(
            crate::question::ChildQuestionAnswerError::Invalid,
        ))
    })?;
    questions
        .answer_child_as_user(&input.parent, &input.child, &input.question_id, &input.text)
        .await
        .map_err(|e| HostError::from(anyhow::Error::new(e)))?;
    Ok(AnsweredResult { answered: true })
}
pub fn answer(ctx: &HostCtx, input: QuestionParams) -> Result<AnswerResult, HostError> {
    ctx.ensure_session(&input.session_id)?;
    if ctx
        .store
        .session(&input.session_id)?
        .parent_session_id
        .is_some()
    {
        return Err(HostError::new(
            409,
            "Child questions must be answered by their direct parent through agent_send",
        ));
    }
    let questions = required(&ctx.questions, "Question service")?;
    ctx.input_limits
        .validate_prompt(&input.text)
        .map_err(|e| HostError::invalid(e.to_string()))?;
    let client = bubblepaw_core::client_param(input.client.as_ref())
        .map_err(|e| HostError::invalid(e.to_string()))?;
    let answer = questions.answer(
        &ctx.harness,
        &input.session_id,
        &input.question_id,
        &input.text,
    )?;
    if !answer.live {
        crate::question::continue_answer(
            &ctx.harness,
            &ctx.store,
            &input.session_id,
            &answer,
            client,
        )?;
    }
    Ok(AnswerResult {
        question: answer.question,
        live: answer.live,
    })
}

pub(super) async fn command(
    ctx: &HostCtx,
    method: MethodId,
    params: Value,
) -> Result<Value, HostError> {
    if method == MethodId::QuestionAnswerChild {
        encode(answer_child(ctx, decode(params)?).await?)
    } else {
        let (ctx, input) = (ctx.clone(), decode(params)?);
        let answer = tokio::task::spawn_blocking(move || answer(&ctx, input))
            .await
            .map_err(|e| HostError::internal(e.to_string()))??;
        encode(answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn root_answer_rejects_child_role_before_optional_question_service() -> anyhow::Result<()>
    {
        let workspace = tempfile::tempdir()?;
        let store = Store::in_memory()?;
        let parent = store.create_session_with_workspace("parent", workspace.path())?;
        let child =
            store.create_session_with_parent("child", Default::default(), Some(&parent.id))?;
        store.update_host_state(&child.id, "fixture", "question", |_| {
            Ok(json!({"id":"pending","status":"pending","text":"answer?"}))
        })?;
        let ctx = HostCtx::for_store(store.clone(), workspace.path());
        let before = store.storage_stats()?;
        let pending = store.host_state(&child.id, "question")?;
        let messages = store.messages(&child.id)?;
        let error = dispatch(
            &ctx,
            Profile::Edit,
            "question.answer",
            json!({"session_id":child.id,"question_id":"pending","text":"yes"}),
        )
        .await
        .unwrap_err();
        assert_eq!(error.http_status(), 409);
        assert!(
            error
                .message
                .starts_with("Child questions must be answered")
        );
        assert_eq!(store.host_state(&child.id, "question")?, pending);
        assert_eq!(store.messages(&child.id)?, messages);
        assert_eq!(
            store.storage_stats()?.transaction_count,
            before.transaction_count
        );
        assert_eq!(
            dispatch(
                &ctx,
                Profile::Edit,
                "question.answer",
                json!({"session_id":parent.id,"question_id":"pending","text":"yes"})
            )
            .await
            .unwrap_err()
            .http_status(),
            503
        );
        Ok(())
    }
}
