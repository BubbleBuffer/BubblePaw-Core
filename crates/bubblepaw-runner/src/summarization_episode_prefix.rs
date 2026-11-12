//! Assemble evidence-bearing episode and chapter prefixes with block provenance.
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
        threshold: usize,
    ) -> Result<Vec<Message>> {
        Ok(self
            .episode_prefix_with_blocks(
                session, history, legacy, episodes, chapters, cutoff, threshold,
            )?
            .0)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn episode_prefix_with_blocks(
        &self,
        session: &str,
        history: &[Message],
        legacy: &[Message],
        episodes: &[ContextEpisode],
        chapters: &[ContextSubjectChapter],
        cutoff: Option<usize>,
        threshold: usize,
    ) -> Result<(Vec<Message>, Vec<ContextBlock>)> {
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
        let opening = history.iter().find(|message| Self::task_anchor(message));
        let mut blocks = prefix
            .iter()
            .map(|message| {
                let original = history.iter().any(|source| source.id == message.id);
                ContextBlock {
                    kind: if opening.is_some_and(|opening| opening.id == message.id) {
                        ContextBlockKind::Opening
                    } else if original {
                        ContextBlockKind::Anchors
                    } else {
                        ContextBlockKind::ChapterSummary
                    },
                    tokens: self.message_tokens(message) as u64,
                    id: None,
                    first_message_id: if original {
                        Some(message.id.clone())
                    } else {
                        None
                    },
                    last_message_id: if original {
                        Some(message.id.clone())
                    } else {
                        None
                    },
                }
            })
            .collect::<Vec<_>>();
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
        let (mut closed_texts, left_out) =
            self.closed_chapter_texts(&covered, chapters, threshold)?;
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
                "[{}] Chapter {} ({}) | {} | episodes {}-{} | related: {}{}\n",
                chapter.short_id(),
                chapter.ordinal,
                chapter.id,
                self.excerpt(
                    description,
                    self.config.lcm.episodes.chapter_description_tokens
                ),
                chapter.first_episode_ordinal,
                last.ordinal,
                chapter.related_ids.join(", "),
                if left_out.contains(chapter.id.as_str()) {
                    " | summary left out of this view; use history_read"
                } else {
                    ""
                }
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
                    "{omitted} navigation rows omitted; {} memory follows.\n",
                    if left_out.is_empty() {
                        "complete chapter"
                    } else {
                        "newer chapter"
                    }
                ));
            }
            let message = summary_block(index);
            blocks.push(ContextBlock {
                kind: ContextBlockKind::MemoryIndex,
                tokens: self.message_tokens(&message) as u64,
                id: None,
                first_message_id: None,
                last_message_id: None,
            });
            prefix.push(message);
        }
        let mut closed = String::new();
        let mut open = String::new();
        let mut closed_spans = Vec::new();
        let mut open_spans = Vec::new();
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
            let spans = if is_closed {
                &mut closed_spans
            } else {
                &mut open_spans
            };
            let whole_chapter = members
                .last()
                .is_some_and(|e| e.ordinal == chapter.last_episode_ordinal);
            if is_closed && whole_chapter {
                // A chapter left out by the budget keeps only its index row.
                let Some(text) = closed_texts.remove(chapter.id.as_str()) else {
                    continue;
                };
                block.push_str(&text);
                spans.push((
                    block.len(),
                    ContextBlock {
                        kind: ContextBlockKind::ChapterSummary,
                        tokens: 0,
                        id: Some(chapter.id.clone()),
                        first_message_id: members
                            .first()
                            .map(|episode| episode.first_message_id.clone()),
                        last_message_id: members
                            .last()
                            .map(|episode| episode.last_message_id.clone()),
                    },
                ));
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
                    spans.push((
                        block.len(),
                        ContextBlock {
                            kind: ContextBlockKind::EpisodeSummary,
                            tokens: 0,
                            id: Some(episode.id.clone()),
                            first_message_id: Some(episode.first_message_id.clone()),
                            last_message_id: Some(episode.last_message_id.clone()),
                        },
                    ));
                }
            }
        }
        if !closed.is_empty() {
            let heading = "Closed chapter summaries (historical):\n";
            let message = summary_block(format!("{heading}{closed}"));
            blocks.extend(self.summary_span_blocks(&message, heading.len(), closed_spans));
            prefix.push(message);
        }
        if !open.is_empty() {
            let heading = "Open chapter episodes (historical):\n";
            let message = summary_block(format!("{heading}{open}"));
            blocks.extend(self.summary_span_blocks(&message, heading.len(), open_spans));
            prefix.push(message);
        }
        Ok((prefix, blocks))
    }

    /// Rendered text of every whole closed chapter, plus the ids the chapter budget
    /// leaves out. The budget is `chapter_budget_percent` of the view threshold; the
    /// oldest chapters go first and the newest always stays. A pure function of the
    /// stored chapters, config and threshold, so a view is rebuilt identically.
    fn closed_chapter_texts<'a>(
        &self,
        covered: &[&ContextEpisode],
        chapters: &'a [ContextSubjectChapter],
        threshold: usize,
    ) -> Result<(HashMap<&'a str, String>, HashSet<&'a str>)> {
        let mut texts = HashMap::new();
        let mut order = Vec::new();
        for chapter in chapters
            .iter()
            .filter(|c| c.status == SubjectChapterStatus::Closed)
        {
            let whole = covered
                .iter()
                .rev()
                .find(|e| e.chapter_id == chapter.id)
                .is_some_and(|e| e.ordinal == chapter.last_episode_ordinal);
            if !whole {
                continue;
            }
            let summary = chapter.summary.as_deref().unwrap_or("");
            if let Some(reason) = crate::episode_models::summary_output_reason("chapter", summary) {
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
            order.push((chapter.id.as_str(), self.tokens(&text)));
            texts.insert(chapter.id.as_str(), text);
        }
        let percent = self.config.lcm.episodes.chapter_budget_percent;
        let mut left_out = HashSet::new();
        if percent > 0 {
            let budget = threshold.saturating_mul(percent) / 100;
            let mut total: usize = order.iter().map(|(_, tokens)| tokens).sum();
            // `order` is oldest first; the last entry is never cut.
            for (id, tokens) in order.iter().take(order.len().saturating_sub(1)) {
                if total <= budget {
                    break;
                }
                total -= tokens;
                left_out.insert(*id);
                texts.remove(id);
            }
        }
        Ok((texts, left_out))
    }
}
