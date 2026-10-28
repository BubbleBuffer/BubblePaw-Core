//! Persisted measurements of an apply, without changing its rendered input.
use super::*;

impl SummarizingContext {
    /// Partition one shared summary message at its original content boundaries.
    /// Count tokens of the actual serialized message once. Tokens crossing a
    /// boundary belong to the earlier span; headings and framing are assigned
    /// once, and all spans sum to the single canonical message estimate.
    pub(super) fn summary_span_blocks(
        &self,
        message: &Message,
        heading_bytes: usize,
        spans: Vec<(usize, ContextBlock)>,
    ) -> Vec<ContextBlock> {
        let body = json!({"role":message.role,"content":message.content,
            "tool_calls":message.tool_calls,"tool_call_id":message.tool_call_id})
        .to_string();
        let content_start =
            body.find("\"content\":").expect("canonical content field") + "\"content\":".len() + 1;
        let encoded = self.tokenizer.encode_ordinary(&body);
        let total = encoded.len();
        let mut byte_offset = 0;
        let starts = self
            .tokenizer
            ._decode_native_and_split(encoded)
            .map(|token| {
                let start = byte_offset;
                byte_offset += token.len();
                start
            })
            .collect::<Vec<_>>();
        let mut previous = 0;
        let mut previous_content = 0;
        let mut escaped_bytes = 0;
        let span_count = spans.len();
        spans
            .into_iter()
            .enumerate()
            .map(|(index, (end, mut block))| {
                let end = heading_bytes + end;
                let escaped = json!(&message.content[previous_content..end]).to_string();
                escaped_bytes += escaped.len() - 2;
                previous_content = end;
                let through = if index + 1 == span_count {
                    total
                } else {
                    starts.partition_point(|start| *start < content_start + escaped_bytes)
                };
                block.tokens = (through - previous + usize::from(index == 0) * 4) as u64;
                previous = through;
                block
            })
            .collect()
    }

    /// Header framing and the raw suffix complete the prefix's block partition.
    /// Provider adapters can estimate more than the canonical message form.
    /// That exact excess is provider framing in Opening, never a proportional
    /// adjustment to summary or raw-message costs. Eight fixed request tokens
    /// remain outside the partition, just as in estimated_input.
    pub(super) fn finish_view_blocks(&self, view: &mut ContextView, request: &ModelRequest) {
        let header =
            self.tokens(&request.system_prompt) + self.tokens(&json!(request.tools).to_string());
        let canonical = header
            + 8
            + request
                .messages
                .iter()
                .map(|message| self.message_tokens(message))
                .sum::<usize>();
        view.blocks.insert(
            0,
            ContextBlock {
                kind: ContextBlockKind::Opening,
                tokens: (header + self.estimated_tokens(request).saturating_sub(canonical)) as u64,
                id: None,
                first_message_id: None,
                last_message_id: None,
            },
        );
        let raw = &request.messages[view.prefix.len()..];
        if let Some(first) = raw.first() {
            view.blocks.push(ContextBlock {
                kind: ContextBlockKind::RawSuffix,
                tokens: raw
                    .iter()
                    .map(|message| self.message_tokens(message) as u64)
                    .sum(),
                id: None,
                first_message_id: Some(first.id.clone()),
                last_message_id: raw.last().map(|message| message.id.clone()),
            });
        }
        view.blocks_estimated = true;
    }

    /// Reconstruct the scripted replay oracle's native view using its exact
    /// source snapshot. Used before and after generated-ID normalization.
    #[cfg(all(test, feature = "bench"))]
    pub(crate) fn episode_view_blocks_for_test(
        &self,
        view: &ContextView,
        request: &ModelRequest,
        history: &[Message],
        episodes: &[ContextEpisode],
        chapters: &[ContextSubjectChapter],
    ) -> Result<Vec<ContextBlock>> {
        // The report contains later gathering too. Reconstruct the records
        // available at this apply snapshot, rather than looking up future
        // episode boundaries in its earlier canonical history.
        let available: HashSet<_> = history.iter().map(|message| message.id.as_str()).collect();
        let episodes: Vec<_> = episodes
            .iter()
            .filter(|episode| {
                available.contains(episode.first_message_id.as_str())
                    && available.contains(episode.last_message_id.as_str())
            })
            .cloned()
            .collect();
        let mut chapters = chapters.to_vec();
        for chapter in &mut chapters {
            let heading = format!("\n## Open chapter {} ({})\n", chapter.ordinal, chapter.id);
            if view
                .prefix
                .iter()
                .any(|message| message.content.contains(&heading))
            {
                // A chapter may close after publication. Its frozen open
                // representation remains the oracle's source of truth.
                chapter.status = SubjectChapterStatus::Open;
            }
        }
        let cutoff = view
            .blocks
            .iter()
            .filter(|block| {
                matches!(
                    block.kind,
                    ContextBlockKind::ChapterSummary | ContextBlockKind::EpisodeSummary
                )
            })
            .filter_map(|block| block.last_message_id.as_deref())
            .map(|id| Self::message_index(history, id))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .max();
        let (prefix, blocks) = self.episode_prefix_with_blocks(
            &view.session_id,
            history,
            &[],
            &episodes,
            &chapters,
            cutoff,
        )?;
        ensure!(
            prefix.len() == view.prefix.len()
                && prefix
                    .iter()
                    .zip(&view.prefix)
                    .all(|(expected, actual)| message_fingerprint(expected)
                        == message_fingerprint(actual)),
            "Replay oracle must reconstruct the actual saved prefix"
        );
        let mut rebuilt = view.clone();
        rebuilt.blocks = blocks;
        self.finish_view_blocks(&mut rebuilt, request);
        ensure!(
            rebuilt.blocks.iter().map(|block| block.tokens).sum::<u64>() + 8
                == self.estimated_tokens(request) as u64,
            "Replay blocks must partition the exact prepared input"
        );
        Ok(rebuilt.blocks)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn applied_event(
        &self,
        session: &str,
        snapshot: &HistorySnapshot,
        view: &ContextView,
        legacy: &[Message],
        input_before: usize,
        input_after: usize,
        trigger_text: String,
        manual: bool,
    ) -> Result<ContextEvent> {
        let last = self
            .store
            .context_view_frontier_message_id(session, view.frontier)?
            .context("Applied view has no canonical coverage")?;
        let end = snapshot.position(&last)? + 1;
        let episodes = self.store.context_episodes(session)?;
        let covered = episodes
            .iter()
            .filter_map(
                |episode| match snapshot.position(&episode.last_message_id) {
                    Ok(index) if index < end => Some(Ok(episode)),
                    Ok(_) => None,
                    Err(error) => Some(Err(error)),
                },
            )
            .collect::<Result<Vec<_>>>()?;
        // Native episodes already recorded this source measurement. An
        // inherited/imported gap has no stored original-source count. Keep the
        // full coverage range and explicitly report that missing measurement;
        // never hydrate cold originals solely to populate a context fact.
        let mut tokens_before = Some(0_u64);
        let mut cursor = 0;
        for episode in &covered {
            let start = snapshot.position(&episode.first_message_id)?;
            if cursor != start {
                tokens_before = None;
            }
            tokens_before =
                tokens_before.map(|tokens| tokens.saturating_add(episode.source_tokens as u64));
            cursor = snapshot.position(&episode.last_message_id)? + 1;
        }
        if cursor != end {
            tokens_before = None;
        }
        let mut tokens_after = legacy
            .iter()
            .map(|message| self.tokens(&message.content) as u64)
            .sum::<u64>();
        let mut summaries = Vec::new();
        for chapter in self.store.context_subject_chapters(session)? {
            let members = covered
                .iter()
                .filter(|episode| episode.chapter_id == chapter.id)
                .copied()
                .collect::<Vec<_>>();
            if members.is_empty() {
                continue;
            }
            if chapter.status == SubjectChapterStatus::Closed
                && members
                    .last()
                    .is_some_and(|episode| episode.ordinal == chapter.last_episode_ordinal)
            {
                tokens_after = tokens_after
                    .saturating_add(self.tokens(chapter.summary.as_deref().unwrap_or("")) as u64);
                summaries.push(chapter.id);
            } else {
                for episode in members {
                    tokens_after =
                        tokens_after.saturating_add(self.tokens(&episode.summary) as u64);
                    summaries.push(episode.id.clone());
                }
            }
        }
        Ok(ContextEvent {
            id: id(),
            session_id: session.into(),
            kind: ContextEventKind::Applied,
            created_at: now(),
            trigger: if manual {
                ContextTrigger::Manual
            } else {
                ContextTrigger::Pressure
            },
            trigger_text,
            first_message_id: snapshot.metadata[0].id.clone(),
            last_message_id: last,
            episode_ids: covered.iter().map(|episode| episode.id.clone()).collect(),
            summary_id: (summaries.len() == 1).then(|| summaries.remove(0)),
            tokens_before,
            tokens_after,
            input_before: Some(input_before as u64),
            input_after: Some(input_after as u64),
            input_estimated: true,
        })
    }
}
