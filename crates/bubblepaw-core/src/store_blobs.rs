//! Blob storage and the message content codec.
use super::*;

impl Store {
    pub(crate) fn blob_put(conn: &Connection, bytes: &[u8]) -> Result<String> {
        Self::put_blob(conn, "blobs", bytes)
    }

    pub(crate) fn blob_get(conn: &Connection, hash: &str) -> Result<Vec<u8>> {
        Self::get_blob(conn, "blobs", hash)
    }

    /// Store checkpoint preimage bytes under their plain SHA-256 in `checkpoint_blobs`.
    pub(super) fn checkpoint_put(conn: &Connection, bytes: &[u8]) -> Result<String> {
        Self::put_blob(conn, "checkpoint_blobs", bytes)
    }

    pub(super) fn checkpoint_get(conn: &Connection, hash: &str) -> Result<Vec<u8>> {
        Self::get_blob(conn, "checkpoint_blobs", hash)
    }

    /// Store `bytes` in `table` (a fixed table name, never user input) under their
    /// plain SHA-256, and return that hash. An existing row is kept as it is.
    fn put_blob(conn: &Connection, table: &str, bytes: &[u8]) -> Result<String> {
        let hash = crate::fingerprint(bytes);
        let exists: bool = conn.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE hash=?1)"),
            [&hash],
            |row| row.get(0),
        )?;
        if !exists {
            let data = zstd::stream::encode_all(bytes, 3)?;
            conn.execute(
                &format!("INSERT INTO {table}(hash,raw_bytes,data) VALUES(?1,?2,?3)"),
                params![hash, bytes.len() as i64, data],
            )?;
        }
        Ok(hash)
    }

    fn get_blob(conn: &Connection, table: &str, hash: &str) -> Result<Vec<u8>> {
        let (length, compressed): (i64, Vec<u8>) = conn.query_row(
            &format!("SELECT raw_bytes,data FROM {table} WHERE hash=?1"),
            [hash],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let bytes = zstd::stream::decode_all(compressed.as_slice())?;
        anyhow::ensure!(
            bytes.len() as i64 == length && crate::fingerprint(&bytes) == hash,
            "Content blob integrity mismatch"
        );
        Ok(bytes)
    }

    pub(super) fn attachment_content_parts(
        conn: &Connection,
        content: &str,
    ) -> Result<Option<Vec<ContentPart>>> {
        // Host envelopes put their delimiters on separate lines. JSON strings
        // escape newlines, so literal tag text inside a file cannot impersonate
        // either boundary.
        let Some((start, body_start, end)) =
            content.rfind("<BubblePawAttachments>\n").and_then(|start| {
                let body_start = start + "<BubblePawAttachments>\n".len();
                content[body_start..]
                    .rfind("\n</BubblePawAttachments>")
                    .map(|end| (start, body_start, body_start + end))
            })
        else {
            return Ok(None);
        };
        let envelope = &content[body_start..end];
        let Ok(value) = serde_json::from_str::<serde_json::Value>(envelope.trim()) else {
            return Ok(None);
        };
        let Some(files) = value.get("files").and_then(|files| files.as_array()) else {
            return Ok(None);
        };
        let mut matches = Vec::new();
        for file in files {
            if let Some(inline) = file
                .get("inline")
                .and_then(|inline| inline.as_str())
                .filter(|inline| inline.len() > 4096)
            {
                let hash = Self::blob_put(conn, inline.as_bytes())?;
                let escaped = serde_json::to_string(inline)?;
                // Match the quoted value including delimiters, but retain the quotes
                // as literals so decoding needs only JSON-escaped inner bytes.
                for (position, _) in content[start..end].match_indices(&escaped) {
                    matches.push((
                        start + position + 1,
                        start + position + escaped.len() - 1,
                        hash.clone(),
                    ));
                }
            }
        }
        if matches.is_empty() {
            return Ok(None);
        }
        matches.sort_by_key(|(start, _, _)| *start);
        let mut offset = 0;
        let mut parts = Vec::new();
        for (start, end, hash) in matches {
            if start < offset {
                continue;
            }
            if start > offset {
                parts.push(ContentPart::Literal {
                    text: content[offset..start].into(),
                });
            }
            parts.push(ContentPart::Blob {
                hash,
                json_escape: true,
            });
            offset = end;
        }
        if offset < content.len() {
            parts.push(ContentPart::Literal {
                text: content[offset..].into(),
            });
        }
        Ok(Some(parts))
    }

    pub(super) fn input_content_ref(
        conn: &Connection,
        text: &str,
    ) -> Result<(String, &'static str)> {
        if let Some(parts) = Self::attachment_content_parts(conn, text)? {
            Ok((Self::blob_put(conn, &serde_json::to_vec(&parts)?)?, "parts"))
        } else {
            Ok((Self::blob_put(conn, text.as_bytes())?, "text"))
        }
    }

    pub(crate) fn input_text(
        conn: &Connection,
        text: &str,
        content_blob: Option<&str>,
        content_format: Option<&str>,
    ) -> Result<String> {
        let Some(hash) = content_blob else {
            return Ok(text.to_owned());
        };
        let bytes = Self::blob_get(conn, hash)?;
        match content_format.unwrap_or("text") {
            "text" => Ok(String::from_utf8(bytes)?),
            "parts" => Self::render_content_parts(conn, serde_json::from_slice(&bytes)?),
            _ => bail!("Unknown accepted input content encoding"),
        }
    }

    fn render_content_parts(conn: &Connection, parts: Vec<ContentPart>) -> Result<String> {
        let mut content = String::new();
        for part in parts {
            match part {
                ContentPart::Literal { text } => content.push_str(&text),
                ContentPart::Blob { hash, json_escape } => {
                    let bytes = String::from_utf8(Self::blob_get(conn, &hash)?)?;
                    if json_escape {
                        let escaped = serde_json::to_string(&bytes)?;
                        content.push_str(&escaped[1..escaped.len() - 1]);
                    } else {
                        content.push_str(&bytes);
                    }
                }
            }
        }
        Ok(content)
    }

    /// The sole archive message-body decoder. Callers that only need metadata
    /// use this boundary without hydrating large content blobs.
    pub(crate) fn decode_message_body(conn: &Connection, data: &str) -> Result<serde_json::Value> {
        cold::decode_body(conn, data)
    }
    pub(crate) fn message_from_data(conn: &Connection, data: &str) -> Result<Message> {
        let mut value = Self::decode_message_body(conn, data)?;
        if let Some(hash) = value
            .get("_provider_state_blob")
            .and_then(|hash| hash.as_str())
        {
            value["provider_state"] = serde_json::from_slice(&Self::blob_get(conn, hash)?)?;
        }
        if let Some(hash) = value.get("_content_blob").and_then(|hash| hash.as_str()) {
            value["content"] =
                serde_json::Value::String(String::from_utf8(Self::blob_get(conn, hash)?)?);
        }
        if let Some(hash) = value
            .get("_content_parts_blob")
            .and_then(|value| value.as_str())
        {
            value["content"] =
                serde_json::Value::String(Self::input_text(conn, "", Some(hash), Some("parts"))?);
        } else if let Some(parts) = value.get("_content_parts") {
            value["content"] = serde_json::Value::String(Self::render_content_parts(
                conn,
                serde_json::from_value(parts.clone())?,
            )?);
        }
        Ok(serde_json::from_value(value)?)
    }

    pub(crate) fn read_message(
        conn: &Connection,
        session_id: &str,
        message_id: &str,
    ) -> Result<Message> {
        let data: String = conn.query_row(
            "SELECT data FROM messages WHERE session_id=?1 AND message_id=?2",
            params![session_id, message_id],
            |row| row.get(0),
        )?;
        Self::message_from_data(conn, &data)
    }
}
