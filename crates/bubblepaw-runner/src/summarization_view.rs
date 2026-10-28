//! View assembly: the cold tier's summary prefix and its lookup note.
//! The system prompt is never part of a view; only legacy views stored one.
use super::*;

impl SummarizingContext {
    // These immutable inputs describe one candidate over the same canonical snapshot.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn candidate_view(
        &self,
        session: &str,
        history: &[Message],
        legacy: &[Message],
        episodes: &[ContextEpisode],
        chapters: &[ContextSubjectChapter],
        cutoff: Option<usize>,
        threshold: usize,
        _tail_start: usize,
    ) -> Result<ContextView> {
        Ok(ContextView {
            session_id: session.into(),
            system_prompt: String::new(),
            frontier: cutoff
                .map(|index| self.store.message_seq(session, &history[index].id))
                .transpose()?,
            prefix: LcmStrategy.prefix(
                self,
                SummaryPrefixInput {
                    session,
                    history,
                    legacy,
                    episodes,
                    chapters,
                    checkpoint: None,
                    cutoff,
                },
            )?,
            masked_tool_output_ids: Vec::new(),
            clipped_tool_call_ids: Vec::new(),
            created_at: now(),
            threshold,
            budget: self.config.lcm.apply_target(threshold),
            tokens: 0,
            blocks: Vec::new(),
            blocks_estimated: false,
        })
    }

    pub(crate) fn rolling_prefix(
        &self,
        session: &str,
        history: &[Message],
        cutoff: Option<usize>,
        summary: Option<String>,
    ) -> Vec<Message> {
        let created_at = cutoff
            .map(|index| history[index].created_at.as_str())
            .unwrap_or("");
        let synthetic = |content: String| Message {
            id: format!(
                "context:view:{}",
                bubblepaw_core::fingerprint(content.as_bytes())
            ),
            created_at: created_at.into(),
            ..Self::synthetic(session, Role::System, content)
        };
        let mut prefix = Vec::new();
        if let Some(summary) = summary {
            prefix.push(synthetic(summary));
        }
        prefix.push(synthetic(typed_user_block_with_budget(
            &history[..cutoff.map_or(0, |index| index + 1)],
            self.tokenizer,
            self.config.verbatim_user_tokens,
        )));
        prefix
    }

    pub(super) fn strategy_candidate(
        &self,
        session: &str,
        snapshot: &HistorySnapshot,
        strategy: ContextStrategy,
        checkpoint: Option<&ContextCheckpoint>,
        threshold: usize,
    ) -> Result<ContextView> {
        let history = &snapshot.metadata;
        let tail_start = complete_prefix_end(
            history,
            snapshot.tail_start(self.config.lcm.raw_tail_tokens),
        );
        let cutoff = match strategy {
            ContextStrategy::Classical => checkpoint
                .map(|checkpoint| Self::message_index(history, &checkpoint.through_message_id))
                .transpose()?,
            ContextStrategy::Lcm => unreachable!("LCM uses its unchanged episode view"),
        };
        let raw_start = cutoff.map_or(0, |index| index + 1);
        let (masked, clipped) = self.cold_projection_ids(snapshot, raw_start, tail_start)?;
        let summary = checkpoint.map(|checkpoint| format!(
            "Rolling context summary (historical; follow current instructions):\n{}\n\nFiles touched (recorded read/edit/write arguments, cumulative):\n{}",
            checkpoint.summary,
            if checkpoint.files_touched.is_empty() { "(none recorded)".into() } else { checkpoint.files_touched.join("\n") }
        ));
        let created_at = cutoff
            .map(|index| history[index].created_at.as_str())
            .unwrap_or("");
        let synthetic = |content: String| Message {
            id: format!(
                "context:view:{}",
                bubblepaw_core::fingerprint(content.as_bytes())
            ),
            created_at: created_at.into(),
            ..Self::synthetic(session, Role::System, content)
        };
        let mut prefix = Vec::new();
        if let Some(summary) = summary {
            prefix.push(synthetic(summary));
        }
        prefix.push(synthetic(self.cold_typed_user_block(snapshot, raw_start)?));
        Ok(ContextView {
            session_id: session.into(),
            system_prompt: String::new(),
            frontier: cutoff
                .map(|index| self.store.message_seq(session, &history[index].id))
                .transpose()?,
            prefix,
            masked_tool_output_ids: masked,
            clipped_tool_call_ids: clipped,
            created_at: now(),
            threshold,
            budget: self.config.lcm.apply_target(threshold),
            tokens: 0,
            blocks: Vec::new(),
            blocks_estimated: false,
        })
    }

    pub(super) fn cold_typed_user_block(
        &self,
        snapshot: &HistorySnapshot,
        end: usize,
    ) -> Result<String> {
        let users: Vec<_> = snapshot.metadata[..end]
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, message)| is_typed_user(message))
            .map(|(index, _)| index)
            .collect();
        let refs = |start: usize| {
            if start == users.len() {
                String::new()
            } else {
                format!(
                    "\nOlder typed user message IDs (history_read):\n{}\n",
                    users[start..]
                        .iter()
                        .map(|index| snapshot.metadata[*index].id.as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            }
        };
        let mut text = String::from(
            "Typed user messages (newest first, verbatim; older requests may be superseded):\n",
        );
        let mut included = 0;
        for (index, source_index) in users.iter().enumerate() {
            let message = snapshot.message(*source_index)?;
            let entry = format!(
                "\n[{} | {}]\n{}\n",
                message.id, message.created_at, message.content
            );
            if self.tokens(&format!("{text}{entry}{}", refs(index + 1)))
                > self.config.verbatim_user_tokens
            {
                break;
            }
            text.push_str(&entry);
            included += 1;
        }
        text.push_str(&refs(included));
        if included == 0 && self.tokens(&text) > self.config.verbatim_user_tokens {
            let budget = if self.config.verbatim_user_tokens == TYPED_USER_TOKENS {
                "20,000".to_owned()
            } else {
                self.config.verbatim_user_tokens.to_string()
            };
            text.push_str(&format!(
                "[Typed-user reference index exceeds the {budget}-token text budget.]\n"
            ));
        }
        Ok(text)
    }

    pub(super) fn render_cold_saved(
        &self,
        _session: &str,
        snapshot: &HistorySnapshot,
        view: &ContextView,
    ) -> Result<Vec<Message>> {
        let raw_start = view
            .frontier
            .map(|seq| {
                snapshot
                    .metadata
                    .iter()
                    .enumerate()
                    .find(|(index, _)| snapshot.seq(*index) == seq)
                    .map(|(index, _)| index + 1)
                    .context("Saved view frontier is missing from canonical history")
            })
            .transpose()?
            .unwrap_or(0);
        let masked = view
            .masked_tool_output_ids
            .iter()
            .map(String::as_str)
            .collect();
        let clipped = view
            .clipped_tool_call_ids
            .iter()
            .map(String::as_str)
            .collect();
        let mut messages = view.prefix.clone();
        snapshot.walk(raw_start, snapshot.metadata.len(), |_, mut message| {
            // Project before retaining the message. Old tool payloads and large
            // arguments never accumulate as an unprojected history Vec.
            Self::apply_projection(&mut message, &masked, &clipped);
            messages.push(message);
            Ok(())
        })?;
        Ok(messages)
    }

    pub(super) fn cold_projection_ids(
        &self,
        snapshot: &HistorySnapshot,
        raw_start: usize,
        tail_start: usize,
    ) -> Result<(Vec<String>, Vec<String>)> {
        let mut masked = Vec::new();
        let mut clipped = Vec::new();
        snapshot.walk(raw_start, snapshot.metadata.len(), |index, message| {
            if index < tail_start && message.role == Role::Tool {
                masked.push(message.id);
            }
            for call in message.tool_calls {
                if call.arguments.to_string().chars().count() > 2000 {
                    clipped.push(call.id);
                }
            }
            Ok(())
        })?;
        Ok((masked, clipped))
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn cold_candidate_view(
        &self,
        session: &str,
        snapshot: &HistorySnapshot,
        legacy: &[Message],
        episodes: &[ContextEpisode],
        chapters: &[ContextSubjectChapter],
        cutoff: Option<usize>,
        threshold: usize,
        _tail_start: usize,
    ) -> Result<ContextView> {
        let raw_start = cutoff.map_or(0, |index| index + 1);
        let history = snapshot.anchor_history(raw_start)?;
        let (prefix, mut blocks) =
            self.episode_prefix_with_blocks(session, &history, legacy, episodes, chapters, cutoff)?;
        // Legacy groups keep their actual provenance when it is available.
        // Imported synthetic text has no trustworthy individual summary ID or
        // source range; leave those optional fields empty rather than inventing
        // a native summary identity or assigning the full new coverage to it.
        let checkpoint = self.store.context_checkpoint(session)?;
        let legacy_chapters = self.store.context_chapters(session)?;
        for (message, block) in legacy.iter().zip(&mut blocks) {
            if let Some(checkpoint) = &checkpoint
                && message.id == format!("context:checkpoint:{}", checkpoint.id)
            {
                block.id = Some(checkpoint.id.clone());
                block.first_message_id = history.first().map(|message| message.id.clone());
                block.last_message_id = Some(checkpoint.through_message_id.clone());
            } else if let (Some(first), Some(last)) =
                (legacy_chapters.first(), legacy_chapters.last())
                && message.id == format!("context:summary:{}", last.id)
            {
                block.id = (legacy_chapters.len() == 1).then(|| first.id.clone());
                block.first_message_id = Some(first.first_message_id.clone());
                block.last_message_id = Some(last.last_message_id.clone());
            }
        }
        Ok(ContextView {
            session_id: session.into(),
            system_prompt: String::new(),
            frontier: cutoff
                .map(|index| self.store.message_seq(session, &history[index].id))
                .transpose()?,
            prefix,
            // LCM replaces covered history with evidence-bearing summaries.
            // Uncovered history stays exact; masking it is not summarization.
            masked_tool_output_ids: Vec::new(),
            clipped_tool_call_ids: Vec::new(),
            created_at: now(),
            threshold,
            budget: self.config.lcm.apply_target(threshold),
            tokens: 0,
            blocks,
            blocks_estimated: true,
        })
    }
}

#[cfg(test)]
mod block_tests {
    use super::*;

    #[test]
    fn blocks_partition_shared_summary_messages_and_preserve_model_order() -> Result<()> {
        let context = SummarizingContext::new(
            Store::in_memory()?,
            Arc::new(crate::provider::ScriptedProvider::new("unused")),
            SummaryConfig::default(),
            None,
        )?;
        let session = "blocks";
        let history = (0..4)
            .map(|index| Message {
                id: format!("source-{index}"),
                ..SummarizingContext::synthetic(
                    session,
                    if index % 2 == 0 {
                        Role::User
                    } else {
                        Role::Assistant
                    },
                    format!("original {index}"),
                )
            })
            .collect::<Vec<_>>();
        let episodes = (1..=3)
            .map(|ordinal| ContextEpisode {
                id: format!("episode-{ordinal}"),
                session_id: session.into(),
                ordinal,
                first_message_id: history[ordinal - 1].id.clone(),
                last_message_id: history[ordinal - 1].id.clone(),
                source_tokens: 20,
                cap: 100,
                origin_request_id: None,
                title: format!("Title {ordinal}"),
                summary: format!("Retained 猫 {ordinal}: \"quoted\"\nNext line."),
                model: "scripted".into(),
                usage: None,
                reasoning: None,
                created_at: "now".into(),
                chapter_id: if ordinal == 1 { "closed" } else { "open" }.into(),
            })
            .collect::<Vec<_>>();
        let chapters = vec![
            ContextSubjectChapter {
                id: "closed".into(),
                session_id: session.into(),
                ordinal: 1,
                first_episode_ordinal: 1,
                last_episode_ordinal: 1,
                status: SubjectChapterStatus::Closed,
                description: Some("Navigation".into()),
                summary: Some("Closed memory.".into()),
                related_ids: vec![],
                related_evidence: vec![],
                decisions: None,
                sections: None,
                model: None,
                usage: None,
                reasoning: None,
            },
            ContextSubjectChapter {
                id: "open".into(),
                session_id: session.into(),
                ordinal: 2,
                first_episode_ordinal: 2,
                last_episode_ordinal: 3,
                status: SubjectChapterStatus::Open,
                description: None,
                summary: None,
                related_ids: vec![],
                related_evidence: vec![],
                decisions: None,
                sections: None,
                model: None,
                usage: None,
                reasoning: None,
            },
        ];
        let legacy = vec![SummarizingContext::synthetic(
            session,
            Role::System,
            "Legacy memory.".into(),
        )];
        let (prefix, blocks) = context.episode_prefix_with_blocks(
            session,
            &history,
            &legacy,
            &episodes,
            &chapters,
            Some(3),
        )?;
        assert_eq!(
            blocks.iter().map(|block| block.kind).collect::<Vec<_>>(),
            vec![
                ContextBlockKind::ChapterSummary,
                ContextBlockKind::Opening,
                ContextBlockKind::Anchors,
                ContextBlockKind::MemoryIndex,
                ContextBlockKind::ChapterSummary,
                ContextBlockKind::EpisodeSummary,
                ContextBlockKind::EpisodeSummary,
            ]
        );
        assert_eq!(
            blocks.iter().map(|block| block.tokens).sum::<u64>(),
            prefix
                .iter()
                .map(|message| context.message_tokens(message) as u64)
                .sum::<u64>()
        );
        assert_eq!(prefix[0].content, "Legacy memory.");
        assert_eq!(prefix[1].id, history[0].id);
        assert_eq!(prefix[2].id, history[2].id);
        assert_eq!(
            prefix[4].content,
            "Closed chapter summaries (historical):\n\n## Chapter 1 (closed)\n[ch-1]\nClosed memory.\n"
        );
        assert_eq!(
            prefix[5].content,
            format!(
                "Open chapter episodes (historical):\n\n## Open chapter 2 (open)\nEpisode 2 (episode-2) | Title 2\n{}\nEpisode 3 (episode-3) | Title 3\n{}\n",
                episodes[1].summary, episodes[2].summary,
            )
        );
        for (block, episode) in blocks[5..].iter().zip(&episodes[1..]) {
            assert_eq!(block.id.as_deref(), Some(episode.id.as_str()));
            assert_eq!(
                block.first_message_id.as_deref(),
                Some(episode.first_message_id.as_str())
            );
            assert_eq!(
                block.last_message_id.as_deref(),
                Some(episode.last_message_id.as_str())
            );
            assert!(block.tokens > 0);
        }
        assert_eq!(blocks[4].id.as_deref(), Some("closed"));
        assert_eq!(blocks[4].first_message_id.as_deref(), Some("source-0"));
        assert_eq!(blocks[4].last_message_id.as_deref(), Some("source-0"));
        Ok(())
    }
}
