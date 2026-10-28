//! Chapter closing. With a confirmed main capture and a chapter role that runs
//! the parent's model with its settings, the chapter is closed by a fork of the
//! main prefix with the chapter task as a tail reminder; otherwise the call is
//! fresh and declares why.
use super::fork_requests::{ForkCall, ForkDelivery, dispatch_fork};
use super::*;
use bubblepaw_core::{ContextState, Reminder};

#[derive(Clone, Debug)]
pub struct ChapterInput {
    pub session_id: String,
    pub episodes: Vec<String>,
    pub previous_chapters: Vec<ContextSubjectChapter>,
    pub typed_user_texts: Vec<String>,
    /// Actual ChildTask instructions, kept separate from human requests.
    pub task_instructions: Vec<String>,
}
#[derive(Clone, Debug)]
pub struct ChapterResult {
    pub description: String,
    pub summary: String,
    pub decisions: Option<Vec<String>>,
    pub sections: Option<Vec<bubblepaw_core::ChapterSection>>,
    pub related_ids: Vec<String>,
    pub related_evidence: Vec<RelatedEvidence>,
    pub model: String,
    pub usage: Usage,
    pub reasoning: Option<String>,
}
pub(super) async fn close_chapter(
    models: &HttpEpisodeModels,
    input: ChapterInput,
    fork: Option<EpisodeFork>,
) -> Result<ChapterResult> {
    let (system, user) = chapter_prompt(models, &input);
    let forked = match &fork {
        Some(fork) => chapter_fork(models, &input.session_id, fork, &system, &user).await?,
        None => None,
    };
    let response = match forked {
        Some(response) => response,
        None => {
            // Without a capture the reason is fixed; otherwise the parent's last
            // request tells whether the route or the settings differ.
            let reason = fork.is_none().then_some(ContextState::SIDE_CALL_NO_CAPTURE);
            models
                .chat("chapter", &input.session_id, system, user, reason)
                .await?
        }
    };
    let (clean, related_ids, related_evidence) = parse_related_with_tasks(
        &response.text,
        &input.previous_chapters,
        &input.typed_user_texts,
        &input.task_instructions,
    );
    let (memory, decisions, sections) =
        if models.episode_config.chapter_prompt == crate::episodes::ChapterPrompt::V2 {
            let metadata = match chapter_sections::parse(&clean, &input.episodes) {
                Ok(metadata) => Some(metadata),
                Err(reason) => {
                    tracing::warn!(
                        event = "chapter_metadata_invalid", session = %input.session_id,
                        reason, "Invalid chapter v2 metadata; retaining ordinary chapter memory"
                    );
                    if let Some(trace) = &models.trace {
                        trace.runtime_info(json!({"event":"chapter_metadata_invalid",
                            "session_id":input.session_id,"reason":reason}));
                    }
                    None
                }
            };
            let (decisions, sections) = metadata
                .map(|metadata| (Some(metadata.decisions), Some(metadata.sections)))
                .unwrap_or_default();
            (chapter_sections::prose(&clean), decisions, sections)
        } else {
            (clean.as_str(), None, None)
        };
    models.validate_summary_output("chapter", &input.session_id, &response, memory)?;
    let (description, summary) = summary_parts(
        memory,
        input
            .typed_user_texts
            .first()
            .or(input.task_instructions.first())
            .map(String::as_str),
    );
    // Size targets guide the model. Never truncate exact facts or retry merely
    // for length; serving budgets bound how much is applied to active context.
    Ok(ChapterResult {
        description,
        summary,
        decisions,
        sections,
        related_ids,
        related_evidence,
        model: response.model,
        usage: response.usage,
        reasoning: response.reasoning,
    })
}

fn chapter_prompt(models: &HttpEpisodeModels, input: &ChapterInput) -> (String, String) {
    let system = format!(
        "{}\n\n{RELATED_INSTRUCTION}",
        models
            .episode_config
            .chapter_prompt
            .text()
            .replace(
                "{limit}",
                &models.episode_config.chapter_summary_max_tokens.to_string()
            )
            .replace(
                "{description_limit}",
                &models.episode_config.chapter_description_tokens.to_string()
            )
    );
    let previous = input
        .previous_chapters
        .iter()
        .filter(|chapter| chapter.status == bubblepaw_core::SubjectChapterStatus::Closed)
        .rev()
        .take(8)
        .map(|chapter| {
            format!(
                "{}:\n# {}",
                chapter.id,
                crate::episodes::token_excerpt(
                    chapter.description.as_deref().unwrap_or(""),
                    models.episode_config.chapter_description_tokens
                )
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let own = input
        .episodes
        .iter()
        .map(|text| format!("BEGIN EPISODE MEMORY\n{text}\nEND EPISODE MEMORY"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let requests = input
        .typed_user_texts
        .iter()
        .map(|text| format!("TYPED REQUEST:\n{text}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let tasks = input
        .task_instructions
        .iter()
        .map(|text| format!("DELEGATED CHILD TASK (not a human statement):\n{text}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let user = format!(
        "EARLIER CHAPTERS (closed):\n{}\n\n{own}\n\nREQUEST/TASK INSTRUCTIONS (read-only evidence):\n{requests}\n{tasks}\n\n{RELATED_INSTRUCTION}",
        if previous.is_empty() {
            "none"
        } else {
            &previous
        }
    );
    (system, user)
}

/// Fork the main prefix when the first available chapter route is the parent's
/// route with the same reasoning and output settings. None sends it fresh.
async fn chapter_fork(
    models: &HttpEpisodeModels,
    session: &str,
    fork: &EpisodeFork,
    system: &str,
    user: &str,
) -> Result<Option<ChatResult>> {
    let (roles, registry) = models.current_models();
    let role = &roles.chapter;
    let Some(candidate) = role
        .models
        .iter()
        .find(|candidate| registry.llm_available(candidate))
    else {
        return Ok(None);
    };
    let parent = &fork.request;
    let delivery = ForkDelivery::new(parent, "submit_chapter");
    let same_settings = Some(role.reasoning_effort) == parent.reasoning_effort
        && role
            .max_output_tokens
            .is_none_or(|max| Some(max) == parent.max_output_tokens);
    if *candidate != parent.model
        || !same_settings
        || !fork.provider.supports_summary_fork(&parent.model)
    {
        return Ok(None);
    }
    let mut request = parent.clone();
    request.output_schema = None;
    let fork_point = request.messages.len();
    request.session_id.clear();
    request.history = Default::default();
    let task = format!("{system}\n\n{user}\n\n{}", delivery.instruction());
    let bytes = bubblepaw_core::prepared_request_bytes(&request)?;
    if bytes + task.len() > fork.max_request_bytes {
        return Ok(None);
    }
    let cold = models
        .context_route
        .fork_cold(session, &fork.request)
        .into_iter()
        .filter(|index| *index < fork_point)
        .collect::<Vec<_>>();
    let model = registry.route(candidate).2;
    let (response, usage) = dispatch_fork(
        models,
        fork,
        session,
        ForkCall {
            delivery,
            request,
            fork_point,
            cold: &cold,
            task: Some(Reminder::new("chapter_task", task)),
            lineage: "chapter_fork",
            kind: "chapter",
            model: &model,
            attempt: 1,
        },
        &|text| {
            let text = if models.episode_config.chapter_prompt == crate::episodes::ChapterPrompt::V2
            {
                chapter_sections::prose(text)
            } else {
                text
            };
            summary_output_reason("chapter", text).is_none()
        },
    )
    .await?;
    let result = ChatResult {
        reasoning: response
            .provider_state
            .as_ref()
            .and_then(ProviderRegistry::reasoning),
        text: response.text,
        model,
        usage,
    };
    models.validate_summary_output_at("chapter", session, &result, &result.text, 1)?;
    Ok(Some(result))
}
