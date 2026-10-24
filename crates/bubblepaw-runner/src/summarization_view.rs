//! View assembly: the cold tier's summary prefix and its lookup note.
//! The system prompt is never part of a view; only legacy views stored one.
use super::*;

impl SummarizingContext {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn episode_prefix(
        &self,
        session: &str,
        history: &[Message],
        legacy: &[Message],
        episodes: &[ContextEpisode],
        chapters: &[ContextSubjectChapter],
        cutoff: Option<usize>,
    ) -> Result<Vec<Message>> {
        let covered = episodes
            .iter()
            .filter_map(
                |episode| match Self::message_index(history, &episode.last_message_id) {
                    Ok(index) if cutoff.is_some_and(|cutoff| index <= cutoff) => Some(Ok(episode)),
                    Ok(_) => None,
                    Err(error) => Some(Err(error)),
                },
            )
            .collect::<Result<Vec<_>>>()?;
        let mut prefix = legacy.to_vec();
        // Preserve the opening request and current task ahead of appended memory.
        // These are original messages, with their original identity and origin.
        prefix.extend(self.task_anchors(history, cutoff));
        // The rendered frozen prefix is the summary block for provider mirrors.
        // Derive synthetic identities from its text, and timestamps from coverage.
        let created_at = covered
            .last()
            .map(|episode| episode.created_at.as_str())
            .or_else(|| cutoff.map(|index| history[index].created_at.as_str()))
            .or_else(|| history.first().map(|message| message.created_at.as_str()))
            .unwrap_or("");
        let summary_block = |content: String| Message {
            id: format!(
                "context:view:{}",
                bubblepaw_core::fingerprint(content.as_bytes())
            ),
            created_at: created_at.into(),
            ..Self::synthetic(session, Role::System, content)
        };
        let mut index_rows = Vec::new();
        let mut index_used = self.tokens("Subject chapter index (historical navigation):\n");
        let mut omitted = 0;
        for chapter in chapters
            .iter()
            .rev()
            .filter(|c| c.status == SubjectChapterStatus::Closed)
        {
            let Some(last) = covered.iter().rev().find(|e| e.chapter_id == chapter.id) else {
                continue;
            };
            let description = if last.ordinal == chapter.last_episode_ordinal {
                chapter.description.as_deref().unwrap_or("")
            } else {
                "Partial coverage; retrieve original episodes"
            };
            let row = format!(
                "[{}] Chapter {} ({}) | {} | episodes {}-{} | related: {}\n",
                chapter.short_id(),
                chapter.ordinal,
                chapter.id,
                self.excerpt(
                    description,
                    self.config.lcm.episodes.chapter_description_tokens
                ),
                chapter.first_episode_ordinal,
                last.ordinal,
                chapter.related_ids.join(", ")
            );
            let cost = self.tokens(&row);
            if index_used + cost <= self.config.lcm.index_tokens {
                index_rows.push(row);
                index_used += cost;
            } else {
                omitted += 1;
            }
        }
        if !index_rows.is_empty() || omitted > 0 {
            index_rows.reverse();
            let mut index = format!(
                "Subject chapter index (historical navigation):\n{}",
                index_rows.concat()
            );
            if omitted > 0 {
                index.push_str(&format!(
                    "{omitted} navigation rows omitted; complete chapter memory follows.\n"
                ));
            }
            prefix.push(summary_block(index));
        }
        let mut closed = String::new();
        let mut open = String::new();
        for chapter in chapters {
            let members: Vec<_> = covered
                .iter()
                .filter(|e| e.chapter_id == chapter.id)
                .copied()
                .collect();
            if members.is_empty() {
                continue;
            }
            let is_closed = chapter.status == SubjectChapterStatus::Closed;
            let block = if is_closed { &mut closed } else { &mut open };
            let whole_chapter = members
                .last()
                .is_some_and(|e| e.ordinal == chapter.last_episode_ordinal);
            if is_closed && whole_chapter {
                let summary = chapter.summary.as_deref().unwrap_or("");
                if let Some(reason) =
                    crate::episode_models::summary_output_reason("chapter", summary)
                {
                    anyhow::bail!(
                        "Context summarization failed: stored chapter {} requires repair ({reason}); view was not changed",
                        chapter.id
                    );
                }
                // Older records acquire their immutable ordinal label only when
                // a new serving view is applied; existing frozen views stay intact.
                let label = format!("[{}]\n", chapter.short_id());
                let label = if summary.starts_with(&label) {
                    ""
                } else {
                    &label
                };
                let text = format!(
                    "\n## Chapter {} ({})\n{}\n",
                    chapter.ordinal,
                    chapter.id,
                    format_args!("{label}{summary}")
                );
                block.push_str(&text);
            } else {
                // A frontier inside a closed chapter must only expose its older episodes.
                let heading = format!(
                    "\n## {} chapter {} ({})\n",
                    if is_closed { "Partial" } else { "Open" },
                    chapter.ordinal,
                    chapter.id
                );
                block.push_str(&heading);
                for episode in members {
                    ensure!(
                        !episode.title.trim().is_empty(),
                        "Context summarization failed: stored episode {} requires repair (empty_title); view was not changed",
                        episode.id
                    );
                    if let Some(reason) =
                        crate::episode_models::summary_output_reason("summary", &episode.summary)
                    {
                        anyhow::bail!(
                            "Context summarization failed: stored episode {} requires repair ({reason}); view was not changed",
                            episode.id
                        );
                    }
                    let title = format!(
                        "Episode {} ({}) | {}\n",
                        episode.ordinal, episode.id, episode.title
                    );
                    let text = format!("{title}{}\n", episode.summary);
                    block.push_str(&text);
                }
            }
        }
        if !closed.is_empty() {
            prefix.push(summary_block(format!(
                "Closed chapter summaries (historical):\n{closed}"
            )));
        }
        if !open.is_empty() {
            prefix.push(summary_block(format!(
                "Open chapter episodes (historical):\n{open}"
            )));
        }
        Ok(prefix)
    }
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
                    history: &history,
                    legacy,
                    episodes,
                    chapters,
                    checkpoint: None,
                    cutoff,
                },
            )?,
            // LCM replaces covered history with evidence-bearing summaries.
            // Uncovered history stays exact; masking it is not summarization.
            masked_tool_output_ids: Vec::new(),
            clipped_tool_call_ids: Vec::new(),
            created_at: now(),
            threshold,
            budget: self.config.lcm.apply_target(threshold),
            tokens: 0,
        })
    }
}
