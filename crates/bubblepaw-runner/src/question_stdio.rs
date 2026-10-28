//! Stdio question answer commands; routing stays in the binary dispatcher.
use super::*;

pub(super) async fn answer(
    harness: &Arc<Harness>,
    store: &Store,
    services: &StdioServices,
    input_limits: &InputLimits,
    method: &str,
    params: &Value,
) -> Result<Value> {
    let control = &services.control;
    let text = || {
        let text = params["text"].as_str().context("text is required")?;
        input_limits.validate_prompt(text)?;
        Ok::<_, anyhow::Error>(text)
    };
    Ok(match method {
        "question.answer_child" => {
            let parent = params["parent"].as_str().context("parent is required")?;
            let child = params["child"].as_str().context("child is required")?;
            control.ensure_workspace(parent)?;
            control.ensure_workspace(child)?;
            let question_id = params["question_id"]
                .as_str()
                .context("question_id is required")?;
            services
                .questions
                .answer_child_as_user(parent, child, question_id, text()?)
                .await?;
            json!({"answered":true})
        }
        "question.answer" => {
            let id = params["session_id"]
                .as_str()
                .context("session_id is required")?;
            control.ensure_workspace(id)?;
            ensure!(
                store.session(id)?.parent_session_id.is_none(),
                "Child questions must be answered by their direct parent through agent_send"
            );
            let origin = bubblepaw_core::client_param(params.get("client"))?;
            let question_id = params["question_id"]
                .as_str()
                .context("question_id is required")?;
            let answer = services
                .questions
                .answer(harness, id, question_id, text()?)?;
            if !answer.live {
                bubblepaw_runner::question::continue_answer(harness, store, id, &answer, origin)?;
            }
            json!({"question":answer.question,"live":answer.live})
        }
        _ => unreachable!("question dispatcher admits only answer commands"),
    })
}
