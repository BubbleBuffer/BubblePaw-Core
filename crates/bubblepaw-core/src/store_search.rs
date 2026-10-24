//! Contentless search stores terms and canonical row references, never text.
use super::*;

type SearchRow = (i64, Message);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub session_id: String,
    pub title: String,
    pub root_id: String,
    pub seq: i64,
    pub message_id: String,
    pub snippet: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchIndexProgress {
    pub indexed_messages: usize,
    pub complete: bool,
}

impl Store {
    pub(super) fn search_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch("CREATE VIRTUAL TABLE IF NOT EXISTS message_search USING fts5(body, content='', contentless_delete=1);
            CREATE TABLE IF NOT EXISTS search_rows(id INTEGER PRIMARY KEY,session_id TEXT NOT NULL,root_id TEXT NOT NULL,seq INTEGER NOT NULL,message_id TEXT NOT NULL,UNIQUE(root_id,seq));
            CREATE INDEX IF NOT EXISTS search_rows_session ON search_rows(session_id);
            CREATE TABLE IF NOT EXISTS search_progress(session_id TEXT PRIMARY KEY,through_seq INTEGER NOT NULL,complete INTEGER NOT NULL DEFAULT 0);")?;
        Ok(())
    }

    pub(super) fn search_rows_after(conn: &Connection, after: i64) -> Result<Vec<SearchRow>> {
        let mut statement =
            conn.prepare("SELECT seq,data FROM messages WHERE seq>?1 ORDER BY seq")?;
        statement
            .query_map([after], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (seq, data) = row?;
                Ok((seq, Self::message_from_data(conn, &data)?))
            })
            .collect()
    }

    fn index_search_rows(conn: &Connection, root: &str, rows: &[SearchRow]) -> Result<()> {
        let mut reference = conn.prepare_cached("INSERT INTO search_rows(session_id,root_id,seq,message_id) VALUES(?1,?2,?3,?4) ON CONFLICT(root_id,seq) DO NOTHING RETURNING id")?;
        let mut terms =
            conn.prepare_cached("INSERT INTO message_search(rowid,body) VALUES(?1,?2)")?;
        for (seq, message) in rows {
            let key: Option<i64> = reference
                .query_row(params![message.session_id, root, seq, message.id], |row| {
                    row.get(0)
                })
                .optional()?;
            if let Some(key) = key {
                // Arguments are canonical searchable content too. The temporary
                // serialization is discarded after FTS tokenization.
                let body = searchable_text(message)?;
                terms.execute(params![key, body])?;
            }
        }
        Ok(())
    }

    pub(super) fn index_rows_best_effort(&self, rows: &[SearchRow], before: i64) {
        if rows.is_empty() {
            return;
        }
        if let Some(host) = self.host()
            && let Some(root) = self.0.path.as_ref().and_then(|path| path.file_stem())
        {
            let new_sessions: Vec<String> = rows
                .iter()
                .map(|(_, message)| message.session_id.clone())
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .filter(|session| {
                    self.read(|conn| {
                        Ok(conn.query_row(
                            "SELECT MIN(seq)>?2 FROM messages WHERE session_id=?1",
                            params![session, before],
                            |row| row.get::<_, bool>(0),
                        )?)
                    })
                    .unwrap_or(false)
                })
                .collect();
            let indexed = host.write(|conn| {
                Self::index_search_rows(conn, &root.to_string_lossy(), rows)?;
                for session in &new_sessions {
                    conn.execute("INSERT INTO search_progress(session_id,through_seq,complete) VALUES(?1,0,1) ON CONFLICT(session_id) DO NOTHING", [session])?;
                }
                for (seq, message) in rows {
                    conn.execute("UPDATE search_progress SET through_seq=MAX(through_seq,?2) WHERE session_id=?1 AND complete=1", params![message.session_id,seq])?;
                }
                Ok(())
            });
            if indexed.is_err() {
                self.invalidate_search_best_effort();
            }
        }
    }

    pub(super) fn invalidate_search_best_effort(&self) {
        if let Some(host) = self.host()
            && let Some(root) = self.0.path.as_ref().and_then(|path| path.file_stem())
        {
            let _ = host.write(|conn| {
                conn.execute("UPDATE search_progress SET complete=0 WHERE session_id IN (SELECT id FROM catalog_projection.sessions WHERE root_id=?1)",[root.to_string_lossy().as_ref()])?;
                Ok(())
            });
        }
    }

    pub(super) fn remove_search_sessions(conn: &Connection, sessions: &[String]) -> Result<()> {
        for session in sessions {
            conn.execute("DELETE FROM message_search WHERE rowid IN (SELECT id FROM search_rows WHERE session_id=?1)", [session])?;
            conn.execute("DELETE FROM search_rows WHERE session_id=?1", [session])?;
            conn.execute("DELETE FROM search_progress WHERE session_id=?1", [session])?;
        }
        Ok(())
    }

    /// Incrementally rebuild missing terms from canonical records. A wiped
    /// serving database starts empty; each call decodes at most max_messages.
    /// Explicit reindex calls are serving steps: a successful call flushes its
    /// written rows and progress once, bounding dirty pages to the caller's budget.
    pub fn reindex_search(
        &self,
        root: Option<&str>,
        max_messages: usize,
    ) -> Result<SearchIndexProgress> {
        let mut wrote = false;
        let progress = self.reindex_search_step(root, max_messages, &mut wrote)?;
        if wrote {
            self.flush_serving()?;
        }
        Ok(progress)
    }

    fn reindex_search_step(
        &self,
        root: Option<&str>,
        max_messages: usize,
        wrote: &mut bool,
    ) -> Result<SearchIndexProgress> {
        anyhow::ensure!(self.is_layout(), "Search requires a directory store");
        let sessions = self.list_sessions()?;
        let mut indexed = 0;
        let mut complete = true;
        for session in sessions {
            let (chat_root, archived, target): (String, bool, Option<i64>) = self
                .0
                .layout
                .as_ref()
                .unwrap()
                .catalog
                .lock()
                .unwrap()
                .query_row(
                    "SELECT root_id,storage_state='archived',(SELECT value FROM metadata WHERE key='search_target:'||sessions.id) FROM sessions WHERE id=?1",
                    [&session.id],
                    |row| {
                        let target: Option<String> = row.get(2)?;
                        Ok((row.get(0)?, row.get(1)?, target.and_then(|value| value.parse().ok())))
                    },
                )?;
            if root.is_some_and(|root| root != chat_root) {
                continue;
            }
            let (after, known_complete) = self.read(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT through_seq,complete FROM search_progress WHERE session_id=?1",
                        [&session.id],
                        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?)),
                    )
                    .optional()?
                    .unwrap_or((0, false)))
            })?;
            // Active records may have appended after the last receipt. Their
            // indexed MAX requires no body decoding. Immutable archives use the
            // FULL transition receipt and remain packed when already complete.
            let active = if archived {
                None
            } else {
                Some(
                    self.scoped(&session.id)?
                        .context("Unknown search session")?,
                )
            };
            let target = if let Some(chat) = &active {
                Some(chat.read(|conn| {
                    Ok(conn.query_row(
                        "SELECT COALESCE(MAX(seq),0) FROM messages WHERE session_id=?1",
                        [&session.id],
                        |row| row.get::<_, i64>(0),
                    )?)
                })?)
            } else {
                target
            };
            if known_complete && target.is_some_and(|target| after >= target) {
                continue;
            }
            let available = max_messages.saturating_sub(indexed);
            if available == 0 {
                complete = false;
                continue;
            }
            let chat = match active {
                Some(chat) => chat,
                None => self
                    .scoped(&session.id)?
                    .context("Unknown search session")?,
            };
            let last = chat.read(|conn| {
                Ok(conn.query_row(
                    "SELECT COALESCE(MAX(seq),0) FROM messages WHERE session_id=?1",
                    [&session.id],
                    |row| row.get::<_, i64>(0),
                )?)
            })?;
            if after >= last {
                self.write(|conn| { conn.execute("INSERT INTO search_progress VALUES(?1,?2,1) ON CONFLICT(session_id) DO UPDATE SET through_seq=excluded.through_seq,complete=1",params![session.id,last])?; Ok(()) })?;
                *wrote = true;
                continue;
            }
            let mut through = after;
            let mut count = 0;
            let walked = chat.for_each_message_range_while(
                &session.id,
                after.saturating_add(1),
                last,
                |seq, message| {
                    // Keep Record -> Serving lock order and one payload live. Each
                    // row has a buffered savepoint, never an application COMMIT.
                    self.write(|conn| {
                        Self::index_search_rows(
                            conn,
                            &chat_root,
                            std::slice::from_ref(&(seq, message)),
                        )
                    })?;
                    *wrote = true;
                    through = seq;
                    count += 1;
                    Ok(count < available)
                },
            )?;
            if walked == 0 {
                through = last;
            }
            self.write(|conn| {
                conn.execute("INSERT INTO search_progress VALUES(?1,?2,?3) ON CONFLICT(session_id) DO UPDATE SET through_seq=excluded.through_seq,complete=excluded.complete", params![session.id, through, through >= last])?;
                Ok(())
            })?;
            *wrote = true;
            indexed += walked;
            if through < last {
                complete = false;
            }
        }
        Ok(SearchIndexProgress {
            indexed_messages: indexed,
            complete,
        })
    }

    /// FTS5 query syntax is accepted. Resolve only matching canonical rows;
    /// snippets are rendered from the decoded message rather than a text copy.
    /// Call reindex_search explicitly after a serving wipe to restore missing
    /// terms. An ordinary indexed query opens only the chats containing hits.
    pub fn search_messages(
        &self,
        query: &str,
        root: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        anyhow::ensure!(self.is_layout(), "Search requires a directory store");
        if query.trim().is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let references: Vec<(String,String,i64,String)> = self.read(|conn| {
            let mut statement = conn.prepare("SELECT r.session_id,r.root_id,r.seq,r.message_id FROM message_search JOIN search_rows r ON r.id=message_search.rowid WHERE message_search MATCH ?1 AND (?2 IS NULL OR r.root_id=?2) ORDER BY bm25(message_search),r.id DESC LIMIT ?3")?;
            Ok(statement.query_map(params![query, root, limit.min(1000) as i64], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)))?.collect::<rusqlite::Result<_>>()?)
        })?;
        let mut hits = Vec::with_capacity(references.len());
        for (session_id, root_id, seq, message_id) in references {
            let chat = self
                .scoped(&session_id)?
                .context("Search record was removed")?;
            let message = chat.read(|conn| {
                let data: Option<String> = conn
                    .query_row(
                        "SELECT data FROM messages WHERE session_id=?1 AND seq=?2",
                        params![session_id, seq],
                        |row| row.get(0),
                    )
                    .optional()?;
                data.map(|data| Self::message_from_data(conn, &data))
                    .transpose()
            })?;
            if let Some(message) = message {
                let text = searchable_text(&message)?;
                hits.push(SearchHit {
                    title: chat.session(&session_id)?.title,
                    session_id,
                    root_id,
                    seq,
                    message_id,
                    snippet: search_snippet(&text, query),
                });
            }
        }
        Ok(hits)
    }
}

fn searchable_text(message: &Message) -> Result<String> {
    let mut body = message.content.clone();
    for call in &message.tool_calls {
        body.push('\n');
        body.push_str(&call.name);
        body.push('\n');
        body.push_str(&serde_json::to_string(&call.arguments)?);
    }
    Ok(body)
}

fn search_snippet(content: &str, query: &str) -> String {
    const LENGTH: usize = 240;
    let term = query
        .split(|c: char| !c.is_alphanumeric())
        .find(|term| !term.is_empty())
        .unwrap_or("");
    let needle: Vec<char> = term.chars().flat_map(char::to_lowercase).take(64).collect();
    let mut window = std::collections::VecDeque::with_capacity(needle.len());
    let mut matched = 0;
    if !needle.is_empty() {
        'find: for (index, ch) in content.chars().enumerate() {
            for lower in ch.to_lowercase() {
                window.push_back((index, lower));
                if window.len() > needle.len() {
                    window.pop_front();
                }
                if window.len() == needle.len() && window.iter().map(|(_, ch)| ch).eq(needle.iter())
                {
                    matched = window.front().map_or(0, |(index, _)| *index);
                    break 'find;
                }
            }
        }
    }
    let start = matched.saturating_sub(60);
    let mut chars = content.chars().skip(start);
    let mut snippet = String::new();
    if start > 0 {
        snippet.push('…');
    }
    snippet.extend(chars.by_ref().take(LENGTH));
    if chars.next().is_some() {
        snippet.push('…');
    }
    snippet
}
