use super::workspace::{string, unsigned, validate_fields};
use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use bubblepaw_core::{
    ContextChapter, ContextEpisode, ContextSubjectChapter, Message, Store, Tool, ToolContext,
    ToolDefinition, ToolRegistry, ToolResult,
};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};

const MAX_BYTES: usize = 60 * 1024;
// Leave room for the JS execution receipt inside its default 10,000-token reply.
const MAX_TOKENS: usize = 9_000;

/// Session archive reads, including in read-only mode.
pub fn register_history_tools(registry: &mut ToolRegistry, store: Store) -> Result<()> {
    for name in ["history_chapters", "history_search", "history_read"] {
        registry.register_host(Arc::new(HistoryTool {
            store: store.clone(),
            name,
        }))?;
    }
    Ok(())
}

struct HistoryTool {
    store: Store,
    name: &'static str,
}
struct ChapterRange {
    chapter: ContextChapter,
    from: usize,
    to: usize,
}
struct EpisodeRange {
    episode: ContextEpisode,
    from: usize,
    to: usize,
}

fn fits(value: &Value) -> Result<bool> {
    let encoded = serde_json::to_string(value)?;
    Ok(encoded.len() <= MAX_BYTES
        && tiktoken_rs::o200k_base_singleton()
            .encode_ordinary(&encoded)
            .len()
            <= MAX_TOKENS)
}

fn integer(input: &Value, key: &str) -> Result<Option<usize>> {
    unsigned(input, key, None)?
        .map(|value| usize::try_from(value).context(format!("{key} is too large")))
        .transpose()
}
fn count(input: &Value, key: &str) -> Result<usize> {
    let count = integer(input, key)?.unwrap_or(20);
    ensure!(count > 0, "{key} must be positive");
    Ok(count.min(50))
}
fn optional_id<'a>(input: &'a Value, key: &str) -> Result<Option<&'a str>> {
    if input.get(key).is_none() {
        return Ok(None);
    }
    let value = string(input, key)?;
    ensure!(!value.is_empty(), "{key} must be a nonempty ID");
    Ok(Some(value))
}

fn selected_range(
    input: &Value,
    legacy: &[ChapterRange],
    chapters: &[ContextSubjectChapter],
    episodes: &[EpisodeRange],
) -> Result<Option<(usize, usize)>> {
    let ordinal = if input["chapter"].is_string() {
        None
    } else {
        integer(input, "chapter")?
    };
    let chapter_id = if input["chapter"].is_string() {
        optional_id(input, "chapter")?
    } else {
        None
    };
    let episode_id = optional_id(input, "episode")?;
    ensure!(
        usize::from(ordinal.is_some())
            + usize::from(chapter_id.is_some())
            + usize::from(episode_id.is_some())
            <= 1,
        "chapter and episode are mutually exclusive"
    );
    if let Some(ordinal) = ordinal {
        let range = legacy.iter().find(|range| range.chapter.ordinal == ordinal)
            .context("chapter must be an existing 1-based legacy ordinal in the calling session; use its string ID for subject chapters")?;
        return Ok(Some((range.from, range.to)));
    }
    if let Some(id) = episode_id {
        let range = episodes
            .iter()
            .find(|range| range.episode.id == id)
            .context("episode must be an existing ID in the calling session")?;
        return Ok(Some((range.from, range.to)));
    }
    if let Some(id) = chapter_id {
        if let Some(range) = legacy.iter().find(|range| range.chapter.id == id) {
            return Ok(Some((range.from, range.to)));
        }
        ensure!(
            chapters.iter().any(|chapter| chapter.id == id),
            "chapter must be an existing ID in the calling session"
        );
        let mut members = episodes
            .iter()
            .filter(|range| range.episode.chapter_id == id);
        let first = members
            .next()
            .context("chapter has no episodes in the calling session")?;
        return Ok(Some((
            first.from,
            members.next_back().map_or(first.to, |last| last.to),
        )));
    }
    Ok(None)
}

/// Search argument keys and values as written, not JSON-escaped (paths, quotes, code).
fn push_values(out: &mut String, value: &Value) {
    match value {
        Value::String(text) => {
            out.push('\n');
            out.push_str(text);
        }
        Value::Array(items) => items.iter().for_each(|item| push_values(out, item)),
        Value::Object(map) => {
            for (key, item) in map {
                out.push('\n');
                out.push_str(key);
                push_values(out, item);
            }
        }
        Value::Null => {}
        other => {
            out.push('\n');
            out.push_str(&other.to_string());
        }
    }
}

fn clip_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

fn snippet(text: &str, terms: &[String]) -> Option<String> {
    let lowercase = text.to_lowercase();
    let offsets = terms
        .iter()
        .map(|term| lowercase.find(term))
        .collect::<Option<Vec<_>>>()?;
    let first = *offsets.iter().min()?;
    // Unicode lowercasing can expand characters, so map the folded byte offset
    // back to an original character rather than slicing by folded bytes.
    let mut folded_bytes = 0;
    let mut original_index: usize = 0;
    for character in text.chars() {
        let width = character.to_lowercase().map(char::len_utf8).sum::<usize>();
        if folded_bytes + width > first {
            break;
        }
        folded_bytes += width;
        original_index += 1;
    }
    Some(
        text.chars()
            .skip(original_index.saturating_sub(100))
            .take(300)
            .collect(),
    )
}

impl HistoryTool {
    fn execute(&self, context: &ToolContext, input: Value) -> Result<Value> {
        validate_fields(
            &input,
            match self.name {
                "history_chapters" => &[],
                "history_search" => &["query", "chapter", "episode", "limit"],
                _ => &[
                    "from",
                    "count",
                    "chapter",
                    "episode",
                    "message_id",
                    "tool_call_id",
                ],
            },
        )?;
        self.store
            .session(&context.session_id)
            .context("calling session does not exist")?;
        let mut message_index = Vec::new();
        self.store
            .for_each_message_index(&context.session_id, |seq, id| {
                message_index.push((seq, id.to_owned()));
                Ok(())
            })?;
        let indexes: HashMap<_, _> = message_index
            .iter()
            .enumerate()
            .map(|(index, (_, id))| (id.as_str(), index))
            .collect();
        let legacy = self
            .store
            .context_chapters(&context.session_id)?
            .into_iter()
            .map(|chapter| {
                let from = *indexes
                    .get(chapter.first_message_id.as_str())
                    .context("chapter start is missing from the calling session")?;
                let to = *indexes
                    .get(chapter.last_message_id.as_str())
                    .context("chapter end is missing from the calling session")?;
                ensure!(from <= to, "chapter range is invalid");
                Ok(ChapterRange { chapter, from, to })
            })
            .collect::<Result<Vec<_>>>()?;
        let chapters = self.store.context_subject_chapters(&context.session_id)?;
        let episodes = self
            .store
            .context_episodes(&context.session_id)?
            .into_iter()
            .map(|episode| {
                let from = *indexes
                    .get(episode.first_message_id.as_str())
                    .context("episode start is missing from the calling session")?;
                let to = *indexes
                    .get(episode.last_message_id.as_str())
                    .context("episode end is missing from the calling session")?;
                ensure!(from <= to, "episode range is invalid");
                Ok(EpisodeRange { episode, from, to })
            })
            .collect::<Result<Vec<_>>>()?;
        match self.name {
            "history_chapters" => {
                let legacy_cutoff = self
                    .store
                    .context_checkpoint(&context.session_id)?
                    .map(|checkpoint| {
                        indexes
                            .get(checkpoint.through_message_id.as_str())
                            .copied()
                            .context("legacy cutoff is missing from the calling session")
                    })
                    .transpose()?;
                let mut entries: Vec<_> = legacy.iter().map(|range| json!({"id":range.chapter.id,"kind":"legacy","status":"legacy","ordinal":range.chapter.ordinal,"title":range.chapter.title,"description":range.chapter.title,"from":range.from,"to":range.to})).collect();
                entries.extend(chapters.iter().map(|chapter| {
                    let members: Vec<_> = episodes.iter().filter(|range| range.episode.chapter_id == chapter.id).map(|range| json!({"id":range.episode.id,"ordinal":range.episode.ordinal,"title":range.episode.title,"from":range.from,"to":range.to})).collect();
                    json!({"id":chapter.id,"short_id":chapter.short_id(),"kind":"subject","ordinal":chapter.ordinal,"status":chapter.status,"description":chapter.description.as_deref().unwrap_or("open"),"first_episode_ordinal":chapter.first_episode_ordinal,"last_episode_ordinal":chapter.last_episode_ordinal,"related_ids":chapter.related_ids,"episodes":members})
                }));
                let result = json!({"chapters":entries,"legacy_cutoff":legacy_cutoff});
                ensure!(
                    fits(&result)?,
                    "complete chapter index exceeds the history output limit (60 KiB / 9,000 tokens)"
                );
                Ok(result)
            }
            "history_search" => {
                let query = string(&input, "query")?;
                let terms: Vec<_> = query.split_whitespace().map(str::to_lowercase).collect();
                ensure!(
                    !terms.is_empty(),
                    "query must contain at least one non-whitespace term"
                );
                let limit = count(&input, "limit")?;
                let selected = selected_range(&input, &legacy, &chapters, &episodes)?;
                let mut result = json!({"matches":[],"more":false});
                let (start, end) = selected.unwrap_or((0, message_index.len().saturating_sub(1)));
                let bounds = message_index.get(start).zip(message_index.get(end));
                if let Some(((first, _), (last, _))) = bounds {
                    self.store.for_each_message_range_while(&context.session_id, *first, *last, |_, message| {
                    let index = indexes[message.id.as_str()];
                    ensure!(!context.cancellation.is_cancelled(), "cancelled");
                    let mut searchable = message.content.clone();
                    for call in &message.tool_calls {
                        push_values(&mut searchable, &call.arguments);
                    }
                    let Some(snippet) = snippet(&searchable, &terms) else {
                        return Ok(true);
                    };
                    let matches = result["matches"].as_array_mut().unwrap();
                    if matches.len() >= limit {
                        result["more"] = json!(true);
                        return Ok(false);
                    }
                    let legacy_chapter = legacy
                        .iter()
                        .find(|range| index >= range.from && index <= range.to)
                        .map(|range| &range.chapter);
                    let episode = episodes
                        .iter()
                        .find(|range| index >= range.from && index <= range.to)
                        .map(|range| &range.episode);
                    let subject = episode.and_then(|episode| {
                        chapters
                            .iter()
                            .find(|chapter| chapter.id == episode.chapter_id)
                    });
                    matches.push(json!({"index":index,"message_id":message.id,"role":message.role,"chapter":legacy_chapter.map(|chapter| chapter.ordinal),"chapter_id":subject.map(|chapter| &chapter.id).or_else(|| legacy_chapter.map(|chapter| &chapter.id)),"chapter_description":subject.map(|chapter| chapter.description.as_deref().unwrap_or("open")).or_else(|| legacy_chapter.map(|chapter| chapter.title.as_str())),"episode_id":episode.map(|episode| &episode.id),"episode_title":episode.map(|episode| &episode.title),"snippet":snippet}));
                    // Reserve the worst-case continuation flag while budgeting.
                    result["more"] = json!(true);
                    if !fits(&result)? {
                        result["matches"].as_array_mut().unwrap().pop();
                        return Ok(false);
                    }
                    result["more"] = json!(false);
                    Ok(true)
                })?;
                }
                Ok(result)
            }
            "history_read" => {
                let message_id = optional_id(&input, "message_id")?;
                let tool_call_id = optional_id(&input, "tool_call_id")?;
                ensure!(
                    message_id.is_none() || tool_call_id.is_none(),
                    "message_id and tool_call_id are mutually exclusive"
                );
                if message_id.is_some() || tool_call_id.is_some() {
                    ensure!(
                        ["from", "count", "chapter", "episode"]
                            .iter()
                            .all(|key| input.get(key).is_none()),
                        "ID lookup cannot be combined with from, count, chapter or episode"
                    );
                    let index = if let Some(id) = message_id {
                        *indexes
                            .get(id)
                            .context("message_id must exist in the calling session")?
                    } else {
                        let id = tool_call_id.unwrap();
                        let mut owner = None;
                        self.store.for_each_message_range_while(&context.session_id, 0, i64::MAX, |_, message| {
                            if message.tool_calls.iter().any(|call| call.id == id) {
                                ensure!(owner.is_none(), "tool_call_id is ambiguous in the calling session; use message_id");
                                owner = Some(indexes[message.id.as_str()]);
                            }
                            Ok(true)
                        })?;
                        owner.context("tool_call_id must identify an original tool call in the calling session")?
                    };
                    let seq = message_index[index].0;
                    let message = self
                        .store
                        .messages_range(&context.session_id, seq, seq, 1)?
                        .pop()
                        .context("message disappeared during history lookup")?
                        .1;
                    return Ok(
                        json!({"messages":[bounded_record(&message, index, None)?],"next":null}),
                    );
                }
                let count = count(&input, "count")?;
                let selected = selected_range(&input, &legacy, &chapters, &episodes)?;
                let from = integer(&input, "from")?
                    .or(selected.map(|(from, _)| from))
                    .context("from is required unless chapter or episode is supplied")?;
                let end = selected.map_or(message_index.len(), |(_, to)| to + 1);
                ensure!(
                    from < message_index.len() || (from == 0 && message_index.is_empty()),
                    "from must be an existing 0-based message index"
                );
                ensure!(
                    selected.is_none_or(|(start, to)| from >= start && from <= to),
                    "from is outside the requested chapter or episode"
                );
                let mut result = json!({"messages":[],"next":null});
                let rows = if from < end {
                    self.store.messages_range(
                        &context.session_id,
                        message_index[from].0,
                        message_index[end - 1].0,
                        count,
                    )?
                } else {
                    Vec::new()
                };
                for (_, message) in rows {
                    let index = indexes[message.id.as_str()];
                    ensure!(!context.cancellation.is_cancelled(), "cancelled");
                    let next = (index + 1 < end).then_some(index + 1);
                    let record = match bounded_record(&message, index, next) {
                        Ok(record) => record,
                        Err(_) if !result["messages"].as_array().unwrap().is_empty() => {
                            result["next"] = json!(index);
                            break;
                        }
                        Err(error) => return Err(error),
                    };
                    result["messages"].as_array_mut().unwrap().push(record);
                    result["next"] = json!(next);
                    if !fits(&result)? {
                        result["messages"].as_array_mut().unwrap().pop();
                        result["next"] = json!(index);
                        break;
                    }
                }
                Ok(result)
            }
            _ => bail!("unknown history tool"),
        }
    }
}

fn bounded_record(message: &Message, index: usize, next: Option<usize>) -> Result<Value> {
    let text = clip_chars(&message.content, 4000);
    let mut record = json!({"index":index,"message_id":message.id,"role":message.role,"text":text,"truncated":message.content.chars().count() > 4000,"tool_calls":message.tool_calls,"tool_call_id":message.tool_call_id});
    if fits(&json!({"messages":[record],"next":next}))? {
        return Ok(record);
    }
    // Text is a documented preview; tool calls and their result links remain exact.
    record["text"] = json!("");
    record["truncated"] = json!(true);
    ensure!(
        fits(&json!({"messages":[record],"next":next}))?,
        "message {index} tool calls or result link exceed the history output limit (60 KiB / 9,000 tokens)"
    );
    let mut low = 0;
    let mut high = text.chars().count();
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        record["text"] = json!(clip_chars(&text, mid));
        if fits(&json!({"messages":[record],"next":next}))? {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    record["text"] = json!(clip_chars(&text, low));
    Ok(record)
}

#[async_trait]
impl Tool for HistoryTool {
    fn definition(&self) -> ToolDefinition {
        let chapter =
            json!({"oneOf":[{"type":"integer","minimum":1},{"type":"string","minLength":1}]});
        let identifier = json!({"type":"string","minLength":1});
        let count = json!({"type":"integer","minimum":1});
        let (description, properties, required) = match self.name {
            "history_chapters" => (
                "List subject chapters (ID, status, description or open, episode ordinal range, related IDs) with their episodes (ID, title, 0-based inclusive message range). V4 chapters are legacy message ranges. Includes legacy_cutoff; summaries omitted.",
                json!({}),
                vec![],
            ),
            "history_search" => (
                "Search session text and original tool arguments for all case-insensitive whitespace terms. Matches name covered episode and chapter, with snippets <=300 characters and more. chapter accepts a chapter ID or legacy numeric ordinal; episode accepts an episode ID. Filters are mutually exclusive. limit defaults to 20, maximum 50.",
                json!({"query":{"type":"string"},"chapter":chapter,"episode":identifier,"limit":count}),
                vec!["query"],
            ),
            _ => (
                "Read original session messages, tool calls and result links, with truncated and next. message_id fetches one original message (including masked output); tool_call_id fetches its original calling message with exact arguments. ID lookup cannot combine with pagination or range filters. Text previews <=4,000 characters; oversized tool calls fail clearly. count defaults to 20, maximum 50. chapter accepts a chapter ID or legacy numeric ordinal; episode accepts an episode ID. These mutually exclusive filters confine the range and default from to its start. All outputs capped at 60 KiB / 9,000 tokens.",
                json!({"from":{"type":"integer","minimum":0},"chapter":chapter,"episode":identifier,"message_id":identifier,"tool_call_id":identifier,"count":count}),
                vec![],
            ),
        };
        ToolDefinition {
            approval: Default::default(),
            name: self.name.into(),
            description: description.into(),
            read_only: true,
            input_schema: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
        }
    }
    async fn invoke(&self, context: ToolContext, input: Value) -> Result<ToolResult> {
        ensure!(!context.cancellation.is_cancelled(), "cancelled");
        let tool = Self {
            store: self.store.clone(),
            name: self.name,
        };
        let value = tokio::task::spawn_blocking(move || tool.execute(&context, input)).await??;
        ensure!(fits(&value)?, "history result exceeds the output limit");
        Ok(ToolResult::text(serde_json::to_string(&value)?))
    }
}
