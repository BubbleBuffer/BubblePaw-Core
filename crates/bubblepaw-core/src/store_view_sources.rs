//! Matching a serving view's text against its source messages.
use super::*;

impl Store {
    pub(super) fn view_sources(
        conn: &Connection,
        session_id: &str,
        prefix: &[Message],
    ) -> Result<ViewSources> {
        Self::archive_current_checkpoint(conn, session_id)?;
        let mut texts = Vec::new();
        for message in prefix {
            let value = serde_json::to_value(message)?;
            for text in [
                message.content.clone(),
                serde_json::to_string(&value["tool_calls"])?,
                serde_json::to_string(&value["provider_state"])?,
            ] {
                if !texts.contains(&text) {
                    texts.push(text);
                }
            }
        }
        let mut sources = ViewSources {
            matches: (0..texts.len())
                .map(|_| std::collections::BTreeMap::new())
                .collect(),
            coverage: (0..texts.len())
                .map(|_| std::collections::BTreeMap::new())
                .collect(),
            texts,
            needles: HashMap::new(),
        };
        for (index, text) in sources.texts.iter().enumerate() {
            for (start, _) in text.char_indices() {
                if let Some(key) = text.as_bytes().get(start..start + 16) {
                    sources
                        .needles
                        .entry(key.try_into().unwrap())
                        .or_default()
                        .push((index, start));
                }
            }
        }
        // Derived candidates are inspected one row at a time. Only matching
        // byte-range descriptors survive; unrelated summary bodies are dropped.
        let mut statement = conn.prepare(
            "SELECT id,summary_hash FROM context_checkpoint_versions WHERE session_id=?1",
        )?;
        for row in statement.query_map([session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })? {
            let (id, hash) = row?;
            let text = String::from_utf8(Self::blob_get(conn, &hash)?)?;
            Self::match_view_source(
                &mut sources,
                "context_checkpoint_versions",
                &id,
                "summary",
                &text,
            );
        }
        for table in [
            "context_chapters",
            "context_episodes",
            "context_subject_chapters",
        ] {
            let mut statement =
                conn.prepare(&format!("SELECT id,data FROM {table} WHERE session_id=?1"))?;
            for row in statement.query_map([session_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })? {
                let (id, data) = row?;
                let value: serde_json::Value = serde_json::from_str(&data)?;
                for field in ["summary", "description", "title"] {
                    if let Some(text) = value
                        .get(field)
                        .and_then(|value| value.as_str())
                        .filter(|text| !text.is_empty())
                    {
                        Self::match_view_source(&mut sources, table, &id, field, text);
                    }
                }
            }
        }
        // The source's retained import summary is its own derived record,
        // including when the host embeds it beside new native summaries.
        for summary in Self::imported_history_seed_at(conn, session_id)?.1 {
            let id = summary
                .id
                .strip_prefix("import-summary:")
                .context("Invalid import summary identity")?;
            Self::match_view_source(
                &mut sources,
                "imported_derived",
                id,
                "summary",
                &summary.content,
            );
        }
        // Supported canonical provenance is an actual prefix message ID or a
        // host-rendered typed-user header: [id | date] / [id]. Older-ID-only
        // indexes do not select bodies. Anonymous custom template text remains
        // literal; it must never cause a whole-history body search.
        let mut selected = std::collections::BTreeSet::new();
        for message in prefix {
            selected.insert(message.id.as_str());
            for candidate in message.content.split('[').skip(1) {
                if let Some((header, _)) = candidate.split_once(']') {
                    let id = header.split_once(" | ").map_or(header, |(id, _)| id);
                    if !id.is_empty() {
                        selected.insert(id);
                    }
                }
            }
        }
        for message_id in selected {
            let canonical: Option<(i64, String)> = conn
                .query_row(
                    "SELECT seq,data FROM messages WHERE session_id=?1 AND message_id=?2",
                    params![session_id, message_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((seq, data)) = canonical else {
                continue;
            };
            let original = Self::message_from_data(conn, &data)?;
            let id = seq.to_string();
            Self::match_view_source(&mut sources, "messages", &id, "content", &original.content);
            let value = serde_json::to_value(original)?;
            for field in ["tool_calls", "provider_state"] {
                if let Some(value) = value.get(field) {
                    Self::match_view_source(
                        &mut sources,
                        "messages",
                        &id,
                        &format!("json:{field}"),
                        &serde_json::to_string(value)?,
                    );
                }
            }
        }
        Ok(sources)
    }

    fn cover_view_text(
        coverage: &mut std::collections::BTreeMap<usize, usize>,
        mut start: usize,
        mut end: usize,
    ) {
        if let Some((&previous, &through)) = coverage.range(..=start).next_back()
            && through >= start
        {
            start = previous;
            end = end.max(through);
            coverage.remove(&previous);
        }
        loop {
            let next = coverage
                .range(start..=end)
                .next()
                .map(|(&start, &end)| (start, end));
            let Some((next, through)) = next else {
                break;
            };
            end = end.max(through);
            coverage.remove(&next);
        }
        coverage.insert(start, end);
    }

    fn match_view_source(
        sources: &mut ViewSources,
        table: &str,
        id: &str,
        field: &str,
        text: &str,
    ) {
        if text.is_empty() {
            return;
        }
        let source = Arc::new(TextSourceRef {
            table: table.into(),
            id: id.into(),
            field: field.into(),
        });
        for (source_start, _) in text.char_indices() {
            if sources.needles.is_empty() {
                break;
            }
            let Some(key) = text.as_bytes().get(source_start..source_start + 16) else {
                break;
            };
            let key: [u8; 16] = key.try_into().unwrap();
            let Some(candidates) = sources.needles.remove(&key) else {
                continue;
            };
            let mut remaining = Vec::new();
            for (index, start) in candidates {
                // Covered positions can later be rendered using a subrange of
                // that pointer, even if another overlapping pointer wins first.
                if sources.coverage[index]
                    .range(..=start)
                    .next_back()
                    .is_some_and(|(&begin, &end)| begin < start && end > start)
                {
                    continue;
                }
                let target = &sources.texts[index][start..];
                let previous = sources.matches[index]
                    .get(&start)
                    .map_or(0, |found| found.length);
                if previous >= target.len() {
                    continue;
                }
                // A later occurrence can only improve the best range if the
                // first as-yet-unmatched byte matches. Repeated long near-matches
                // therefore avoid repeatedly comparing their whole prefix.
                if previous > 0
                    && target.as_bytes().get(previous)
                        != text.as_bytes().get(source_start + previous)
                {
                    remaining.push((index, start));
                    continue;
                }
                let mut length = target
                    .as_bytes()
                    .iter()
                    .zip(&text.as_bytes()[source_start..])
                    .take_while(|(left, right)| left == right)
                    .count();
                while !target.is_char_boundary(length) {
                    length -= 1;
                }
                if length > previous {
                    sources.matches[index].insert(
                        start,
                        ViewTextMatch {
                            source: source.clone(),
                            start: source_start,
                            length,
                        },
                    );
                    Self::cover_view_text(&mut sources.coverage[index], start, start + length);
                }
                if length.max(previous) < target.len() {
                    remaining.push((index, start));
                }
            }
            if !remaining.is_empty() {
                sources.needles.insert(key, remaining);
            }
        }
        if text.len() < 16 {
            for (index, target) in sources.texts.iter().enumerate() {
                for (start, _) in target.match_indices(text) {
                    let matches = &mut sources.matches[index];
                    if matches
                        .get(&start)
                        .is_none_or(|previous| text.len() > previous.length)
                    {
                        matches.insert(
                            start,
                            ViewTextMatch {
                                source: source.clone(),
                                start: 0,
                                length: text.len(),
                            },
                        );
                        Self::cover_view_text(
                            &mut sources.coverage[index],
                            start,
                            start + text.len(),
                        );
                    }
                }
            }
        }
    }

    pub(super) fn text_pointers(text: &str, sources: &ViewSources) -> Vec<TextPointer> {
        let index = sources
            .texts
            .iter()
            .position(|target| target == text)
            .unwrap();
        let mut matches = sources.matches[index].iter().peekable();
        let mut best: Option<(usize, &ViewTextMatch)> = None;
        let mut pointers = Vec::new();
        let mut offset = 0;
        let mut literal_start = 0;
        while offset < text.len() {
            while let Some((&start, found)) = matches.peek().copied()
                && start <= offset
            {
                if best.is_none_or(|(previous, old)| start + found.length > previous + old.length) {
                    best = Some((start, found));
                }
                matches.next();
            }
            if let Some((start, found)) = best
                && start + found.length > offset
            {
                if literal_start < offset {
                    pointers.push(TextPointer::Literal {
                        text: text[literal_start..offset].into(),
                    });
                }
                pointers.push(TextPointer::Source {
                    table: found.source.table.clone(),
                    id: found.source.id.clone(),
                    field: found.source.field.clone(),
                    start: found.start + offset - start,
                    end: found.start + found.length,
                });
                offset = start + found.length;
                literal_start = offset;
            } else {
                offset += text[offset..].chars().next().unwrap().len_utf8();
            }
        }
        if literal_start < text.len() {
            pointers.push(TextPointer::Literal {
                text: text[literal_start..].into(),
            });
        }
        pointers
    }
}
