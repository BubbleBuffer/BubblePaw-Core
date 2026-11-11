//! Encoding and decoding a serving view with text pointers.
use super::*;

impl Store {
    pub(super) fn encode_context_view(conn: &Connection, view: &ContextView) -> Result<String> {
        let sources = Self::view_sources(conn, &view.session_id, &view.prefix)?;
        let mut pointers = Vec::new();
        for message in &view.prefix {
            let canonical: Option<(i64, String)> = conn
                .query_row(
                    "SELECT seq,data FROM messages WHERE session_id=?1 AND message_id=?2",
                    params![view.session_id, message.id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((seq, data)) = &canonical
                && serde_json::to_vec(&Self::message_from_data(conn, data)?)?
                    == serde_json::to_vec(message)?
            {
                // Global seq gaps from other sessions do not change this range.
                if let Some(PrefixPointer::Messages { last, .. }) = pointers.last_mut() {
                    let next: Option<i64> = conn.query_row(
                        "SELECT MIN(seq) FROM messages WHERE session_id=?1 AND seq>?2",
                        params![view.session_id, *last],
                        |row| row.get(0),
                    )?;
                    if next == Some(*seq) {
                        *last = *seq;
                        continue;
                    }
                }
                pointers.push(PrefixPointer::Messages {
                    first: *seq,
                    last: *seq,
                });
                continue;
            }
            let mut metadata = serde_json::to_value(message)?;
            let mut fields = HashMap::new();
            let original = canonical
                .as_ref()
                .map(|(_, data)| -> Result<_> {
                    Ok(serde_json::to_value(Self::message_from_data(conn, data)?)?)
                })
                .transpose()?;
            for field in ["tool_calls", "provider_state"] {
                let value = metadata
                    .as_object_mut()
                    .unwrap()
                    .remove(field)
                    .unwrap_or(serde_json::Value::Null);
                let inherited = original
                    .as_ref()
                    .and_then(|value| value.get(field))
                    .unwrap_or(&serde_json::Value::Null);
                // Value equality collapses signed zero. A frozen projection must
                // preserve the exact JSON spelling used by its provider request.
                if serde_json::to_vec(&value)? != serde_json::to_vec(inherited)? {
                    fields.insert(
                        field.to_owned(),
                        Self::text_pointers(&serde_json::to_string(&value)?, &sources),
                    );
                }
            }
            metadata.as_object_mut().unwrap().remove("content");
            pointers.push(PrefixPointer::Rendered {
                base_seq: canonical.map(|(seq, _)| seq),
                metadata,
                content: Self::text_pointers(&message.content, &sources),
                fields,
            });
        }
        let mut value = serde_json::to_value(view)?;
        value.as_object_mut().unwrap().remove("prefix");
        value["prefix_refs"] = serde_json::to_value(pointers)?;
        Ok(serde_json::to_string(&value)?)
    }

    pub(super) fn decode_context_view(conn: &Connection, data: &str) -> Result<ContextView> {
        let mut value: serde_json::Value = serde_json::from_str(data)?;
        let Some(refs) = value.get("prefix_refs") else {
            return Ok(serde_json::from_value(value)?);
        };
        let pointers: Vec<PrefixPointer> = serde_json::from_value(refs.clone())?;
        let session_id = value["session_id"]
            .as_str()
            .context("View has no session")?;
        let mut prefix = Vec::new();
        for pointer in pointers {
            match pointer {
                PrefixPointer::Messages { first, last } => {
                    let mut statement = conn.prepare("SELECT data FROM messages WHERE session_id=?1 AND seq BETWEEN ?2 AND ?3 ORDER BY seq")?;
                    for row in statement.query_map(params![session_id, first, last], |row| {
                        row.get::<_, String>(0)
                    })? {
                        prefix.push(Self::message_from_data(conn, &row?)?);
                    }
                }
                PrefixPointer::Rendered {
                    base_seq,
                    metadata,
                    content,
                    fields,
                } => {
                    let mut value = if let Some(seq) = base_seq {
                        let data: String = conn.query_row(
                            "SELECT data FROM messages WHERE session_id=?1 AND seq=?2",
                            params![session_id, seq],
                            |row| row.get(0),
                        )?;
                        let mut original =
                            serde_json::to_value(Self::message_from_data(conn, &data)?)?;
                        original
                            .as_object_mut()
                            .unwrap()
                            .retain(|field, _| field == "tool_calls" || field == "provider_state");
                        original
                    } else {
                        serde_json::json!({})
                    };
                    value.as_object_mut().unwrap().extend(
                        metadata
                            .as_object()
                            .context("Invalid rendered message metadata")?
                            .clone(),
                    );
                    value["content"] = serde_json::Value::String(Self::render_text_pointers(
                        conn, session_id, content,
                    )?);
                    for (field, pointers) in fields {
                        value[&field] = serde_json::from_str(&Self::render_text_pointers(
                            conn, session_id, pointers,
                        )?)?;
                    }
                    prefix.push(serde_json::from_value(value)?);
                }
            }
        }
        value["prefix"] = serde_json::to_value(prefix)?;
        Ok(serde_json::from_value(value)?)
    }

    fn render_text_pointers(
        conn: &Connection,
        session_id: &str,
        pointers: Vec<TextPointer>,
    ) -> Result<String> {
        let mut rendered = String::new();
        for pointer in pointers {
            match pointer {
                TextPointer::Literal { text } => rendered.push_str(&text),
                TextPointer::Source {
                    table,
                    id,
                    field,
                    start,
                    end,
                } => {
                    let text = if table == "messages" {
                        let data: String = conn.query_row(
                            "SELECT data FROM messages WHERE session_id=?1 AND seq=?2",
                            params![session_id, id.parse::<i64>()?],
                            |row| row.get(0),
                        )?;
                        let message = Self::message_from_data(conn, &data)?;
                        if let Some(field) = field.strip_prefix("json:") {
                            serde_json::to_string(
                                &serde_json::to_value(message)?
                                    .get(field)
                                    .context("View message field is absent")?,
                            )?
                        } else {
                            message.content
                        }
                    } else if table == "imported_derived" {
                        anyhow::ensure!(field == "summary", "Unknown imported view source field");
                        let data: String = conn.query_row(
                            "SELECT data FROM imported_derived WHERE session_id=?1 AND id=?2",
                            params![session_id, id],
                            |row| row.get(0),
                        )?;
                        let record: crate::transcript::ImportDerivedRecord =
                            serde_json::from_str(&data)?;
                        Self::import_summary_text(&record)
                            .context("Imported summary source has no text")?
                    } else if table == "context_checkpoint_versions" {
                        let hash: String = conn.query_row("SELECT summary_hash FROM context_checkpoint_versions WHERE session_id=?1 AND id=?2", params![session_id, id], |row| row.get(0))?;
                        String::from_utf8(Self::blob_get(conn, &hash)?)?
                    } else {
                        anyhow::ensure!(
                            [
                                "context_chapters",
                                "context_episodes",
                                "context_subject_chapters"
                            ]
                            .contains(&table.as_str()),
                            "Invalid view source table"
                        );
                        let data: String = conn.query_row(
                            &format!("SELECT data FROM {table} WHERE session_id=?1 AND id=?2"),
                            params![session_id, id],
                            |row| row.get(0),
                        )?;
                        let source: serde_json::Value = serde_json::from_str(&data)?;
                        source
                            .get(&field)
                            .and_then(|field| field.as_str())
                            .context("View source text is absent")?
                            .to_owned()
                    };
                    rendered.push_str(
                        text.get(start..end)
                            .context("Invalid view text source range")?,
                    );
                }
            }
        }
        Ok(rendered)
    }
}
