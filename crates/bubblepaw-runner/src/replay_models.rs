//! Scripted replay adapters.
use super::*;

#[async_trait]
impl EpisodeModels for ReplayModels {
    async fn decide(&self, input: SplitInput) -> Result<SplitResult> {
        self.begin("decision")?;
        let result = if let Some(inner) = &self.inner {
            inner.decide(input).await?
        } else {
            let chosen = input
                .options
                .iter()
                .position(|o| o.user.contains("[new subject]"));
            let mut probabilities = vec![0.0; input.options.len() + 1];
            probabilities[chosen.unwrap_or(input.options.len())] = 1.0;
            let usage = Usage {
                input_tokens: token_count(&format!(
                    "{}\n{}\n{}",
                    crate::episode_models::SPLIT_QUESTION,
                    input.open_chapter,
                    input
                        .options
                        .iter()
                        .map(|option| format!(
                            "{} {} {}",
                            option.message_id, option.agent_before, option.user
                        ))
                        .collect::<Vec<_>>()
                        .join("\n")
                ))? as u64,
                ..Usage::default()
            };
            self.fake_call(&usage);
            SplitResult {
                choice: chosen.map(|i| input.options[i].message_id.clone()),
                probabilities,
                model: "scripted-jev".into(),
                usage,
                reasoning: None,
            }
        };
        Ok(result)
    }
    async fn summarize(&self, input: SummaryInput) -> Result<SummaryResult> {
        self.begin("summary")?;
        self.record_kind(&input.session_id)?;
        if let Some(inner) = &self.inner {
            return inner.summarize(input).await;
        }
        let summary = scripted_episode_memory(&input.episode_text, input.cap);
        let mut system = crate::episode_models::SUMMARY_PROMPT
            .replace("{target}", &input.cap.to_string())
            .replace("{limit}", &input.cap.to_string());
        if let Some(focus) = input
            .focus
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            system.push_str(&format!("\n\nFor this manual compaction, preserve the details needed for this user focus while retaining the summary requirements above:\n{focus}"));
        }
        let turn = input
            .turn_request
            .as_ref()
            .map(|text| format!("TASK ANCHOR (read-only, may be a child task):\n{text}\n\n"))
            .unwrap_or_default();
        let user = format!(
            "{turn}PREVIOUS EPISODE (read-only):\n{}\n\nCURRENT EPISODE:\n{}\n\n{}",
            input.previous_summary.as_deref().unwrap_or("none"),
            input.episode_text,
            crate::episode_models::EPISODE_SUMMARY_CLOSE
        );
        let usage = Usage {
            input_tokens: role_input_tokens(&system, &user)?,
            output_tokens: token_count(&summary)? as u64,
            ..Usage::default()
        };
        self.fake_call(&usage);
        Ok(SummaryResult {
            title: "Scripted replay episode".into(),
            summary,
            model: "scripted-flash".into(),
            usage,
            reasoning: None,
        })
    }
    async fn close_chapter(&self, input: ChapterInput) -> Result<ChapterResult> {
        self.begin("chapter")?;
        if let Some(inner) = &self.inner {
            return inner.close_chapter(input).await;
        }
        let previous = input
            .previous_chapters
            .iter()
            .filter(|chapter| chapter.status == bubblepaw_core::SubjectChapterStatus::Closed)
            .rev()
            .take(8)
            .map(|chapter| {
                format!(
                    "{}:\n# {}\n{}",
                    chapter.id,
                    chapter.description.as_deref().unwrap_or(""),
                    chapter.summary.as_deref().unwrap_or("")
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let own = input
            .episodes
            .iter()
            .enumerate()
            .map(|(i, text)| format!("EPISODE {i}:\n{text}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let user = format!(
            "EARLIER CHAPTERS (closed):\n{}\n\n{own}\n\n{}",
            if previous.is_empty() {
                "none"
            } else {
                &previous
            },
            crate::episode_models::RELATED_INSTRUCTION
        );
        let summary = input.episodes.join("\n\n");
        let usage = Usage {
            input_tokens: role_input_tokens(crate::episode_models::CHAPTER_PROMPT, &user)?,
            output_tokens: token_count(&summary)? as u64,
            ..Usage::default()
        };
        self.fake_call(&usage);
        Ok(ChapterResult {
            description: "Scripted replay chapter".into(),
            summary,
            related_ids: vec![],
            related_evidence: vec![],
            decisions: None,
            sections: None,
            model: "scripted-flash".into(),
            usage,
            reasoning: None,
        })
    }

    async fn summarize_classical(&self, input: ClassicalInput) -> Result<ClassicalResult> {
        self.begin("summary")?;
        if let Some(inner) = &self.inner {
            return inner.summarize_classical(input).await;
        }
        let summary = format!(
            "## Latest user request (verbatim)\n{}\n\n## Goal\nScripted replay.\n\n## Constraints and decisions\n(none)\n\n## Done (dated, past tense)\n{}\n\n## In progress\n{}\n\n## Stale or superseded asks\n(none)\n\n## Next step\nContinue the recorded chat.",
            input.latest_user_request.as_deref().unwrap_or("(none)"),
            if input.previous_summary.is_some() {
                "The prior recorded span was incorporated."
            } else {
                "(none)"
            },
            input.span_text.chars().take(1000).collect::<String>(),
        );
        let usage = Usage {
            input_tokens: (token_count(crate::episode_models::CLASSICAL_PROMPT)?
                + token_count(&format!(
                    "PREVIOUS SUMMARY (read-only):\n{}\n\nLATEST TYPED USER REQUEST (verbatim, read-only):\n{}\n\nNEW HISTORY SPAN (read-only):\n{}",
                    input.previous_summary.as_deref().unwrap_or("none"),
                    input.latest_user_request.as_deref().unwrap_or("none"),
                    input.span_text,
                ))?) as u64,
            output_tokens: token_count(&summary)? as u64,
            ..Usage::default()
        };
        self.fake_call(&usage);
        Ok(ClassicalResult {
            summary,
            model: "scripted-flash".into(),
            usage,
            reasoning: None,
        })
    }
}
