//! Cold canonical rows and verified, whole-chat file compression.
use super::*;
use base64::Engine;
use std::{
    collections::HashSet,
    io::{Read, Write},
};

const CODEC: &str = "zstd-dict-v1";
const DICTIONARY_BYTES: usize = 16 * 1024;
const SAMPLE_BYTES: usize = 1024 * 1024;
pub(super) struct Dictionary {
    bytes: Vec<u8>,
}
pub(super) type DictionaryPins = Arc<Mutex<HashMap<String, Arc<Dictionary>>>>;
type DictionaryCache = Mutex<HashMap<String, Weak<Dictionary>>>;
static DICTIONARIES: OnceLock<DictionaryCache> = OnceLock::new();

fn dictionary(conn: &Connection, hash: &str) -> Result<Arc<Dictionary>> {
    let cache = DICTIONARIES.get_or_init(Mutex::default);
    {
        let mut cache = cache.lock().unwrap();
        if let Some(dictionary) = cache.get(hash).and_then(Weak::upgrade) {
            return Ok(dictionary);
        }
        cache.retain(|_, dictionary| dictionary.strong_count() > 0);
    }
    let bytes: Vec<u8> = conn.query_row(
        "SELECT data FROM message_dictionaries WHERE hash=?1",
        [hash],
        |row| row.get(0),
    )?;
    anyhow::ensure!(
        format!("{:x}", Sha256::digest(&bytes)) == hash,
        "Corrupt message dictionary"
    );
    let mut cache = cache.lock().unwrap();
    cache.retain(|_, dictionary| dictionary.strong_count() > 0);
    // Concurrent first loads converge on the same pinned value. Otherwise a
    // registry entry could outlive one loader while overlooking another pin.
    if let Some(dictionary) = cache.get(hash).and_then(Weak::upgrade) {
        return Ok(dictionary);
    }
    let dictionary = Arc::new(Dictionary { bytes });
    cache.insert(hash.to_owned(), Arc::downgrade(&dictionary));
    Ok(dictionary)
}

pub(super) fn pin_dictionary(
    conn: &Connection,
    pins: &DictionaryPins,
    hash: &str,
) -> Result<Arc<Dictionary>> {
    if let Some(dictionary) = pins.lock().unwrap().get(hash).cloned() {
        return Ok(dictionary);
    }
    let dictionary = dictionary(conn, hash)?;
    pins.lock()
        .unwrap()
        .insert(hash.to_owned(), dictionary.clone());
    Ok(dictionary)
}

pub(super) fn pin_dictionaries(conn: &Connection) -> Result<DictionaryPins> {
    let pins = Arc::default();
    pin_all_into(conn, &pins)?;
    Ok(pins)
}

pub(super) fn pin_all_into(conn: &Connection, pins: &DictionaryPins) -> Result<()> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='message_dictionaries')",
        [],
        |row| row.get(0),
    )?;
    if exists {
        let mut statement = conn.prepare("SELECT hash FROM message_dictionaries")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            pin_dictionary(conn, pins, &row.get::<_, String>(0)?)?;
        }
    }
    Ok(())
}

pub(super) fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS message_dictionaries(hash TEXT PRIMARY KEY,data BLOB NOT NULL);")?;
    Ok(())
}
fn latest_frontier(conn: &Connection, session: &str) -> Result<Option<i64>> {
    let mut frontier:Option<i64>=conn.query_row("SELECT MAX(boundary) FROM (SELECT through_message_seq AS boundary FROM context_checkpoints WHERE session_id=?1 UNION ALL SELECT last_message_seq FROM context_chapters WHERE session_id=?1 UNION ALL SELECT last_message_seq FROM context_episodes WHERE session_id=?1)",[session],|row|row.get(0))?;
    let boundary = Store::imported_history_seed_at(conn, session)?.0;
    frontier = frontier.into_iter().chain(boundary).max();
    Ok(frontier)
}

#[derive(Serialize, Deserialize)]
struct CompressionPass {
    policy: String,
    through: i64,
    has_dictionary: bool,
    minimum_messages: u32,
    minimum_bytes: u64,
}

fn compression_policy_signature(
    conn: &Connection,
    session: &str,
    frontier: i64,
    view: Option<&serde_json::Value>,
    options: StoreOptions,
) -> Result<String> {
    let mut versions = Vec::new();
    for (table, boundary) in [
        ("context_checkpoints", "through_message_seq"),
        ("context_chapters", "last_message_seq"),
        ("context_episodes", "last_message_seq"),
    ] {
        let version:Option<(String,i64)>=conn.query_row(&format!("SELECT id,{boundary} FROM {table} WHERE session_id=?1 ORDER BY rowid DESC LIMIT 1"),[session],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
        if let Some((id, boundary)) = version {
            versions.push((table.to_owned(), id, boundary));
        }
    }
    let imported: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='imported_derived')",
        [],
        |row| row.get(0),
    )?;
    if imported {
        let version:Option<(String,i64)>=conn.query_row("SELECT id,boundary_seq FROM imported_derived WHERE session_id=?1 AND kind IN ('codex_compaction','claude_compaction_summary') AND boundary_seq IS NOT NULL ORDER BY source_line DESC,id DESC LIMIT 1",[session],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
        if let Some((id, boundary)) = version {
            versions.push(("imported_derived".into(), id, boundary));
        }
    }
    // Only canonical view references affect cold eligibility. Token estimates,
    // prompts and generated timestamps do not cause another historical scan.
    let references=view.map(|view|serde_json::json!({"frontier":view["frontier"],"refs":view.get("prefix_refs"),"legacy_ids":view.get("prefix").and_then(|prefix|prefix.as_array()).map(|prefix|prefix.iter().filter_map(|message|message["id"].as_str()).collect::<Vec<_>>())}));
    let input = serde_json::json!([
        CODEC,
        frontier,
        versions,
        references,
        options.cold_tail_messages,
        options.cold_min_messages,
        options.cold_min_bytes,
        options.cold_row_min_bytes
    ]);
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(&input)?)))
}

pub(super) fn decode_body(conn: &Connection, data: &str) -> Result<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(data)?;
    let Some(codec) = value.get("_row_codec").and_then(|v| v.as_str()) else {
        return Ok(value);
    };
    anyhow::ensure!(codec == CODEC, "Unknown canonical message codec");
    let hash = value["dictionary"]
        .as_str()
        .context("Missing message dictionary")?;
    let dictionary = dictionary(conn, hash)?;
    let expected = usize::try_from(
        value["raw_bytes"]
            .as_u64()
            .context("Missing cold row size")?,
    )?;
    anyhow::ensure!(
        expected <= 64 * 1024 * 1024,
        "Cold message exceeds decoder limit"
    );
    let encoded = value["data"]
        .as_str()
        .context("Missing compressed message")?;
    let compressed = base64::engine::general_purpose::STANDARD.decode(encoded)?;
    let bytes = zstd::bulk::Decompressor::with_dictionary(&dictionary.bytes)?
        .decompress(&compressed, expected)?;
    anyhow::ensure!(bytes.len() == expected, "Cold message size mismatch");
    let expected_hash = value["sha256"]
        .as_str()
        .context("Missing cold message checksum")?;
    anyhow::ensure!(
        format!("{:x}", Sha256::digest(&bytes)) == expected_hash,
        "Cold message checksum mismatch"
    );
    Ok(serde_json::from_slice(&bytes)?)
}

impl Store {
    /// Global sequence bounds and count, without decoding message bodies.
    pub fn message_bounds(&self, session: &str) -> Result<(Option<i64>, Option<i64>, u64)> {
        if let Some(store) = self.scoped(session)? {
            return store.message_bounds(session);
        }
        self.read(|conn| {
            Ok(conn.query_row(
                "SELECT MIN(seq),MAX(seq),COUNT(*) FROM messages WHERE session_id=?1",
                [session],
                |row| Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)? as u64)),
            )?)
        })
    }

    /// Walk canonical IDs in order without parsing or retaining message bodies.
    /// The callback must not reenter this Store while its read lock is held.
    pub fn for_each_message_index(
        &self,
        session: &str,
        mut visit: impl FnMut(i64, &str) -> Result<()>,
    ) -> Result<usize> {
        if let Some(store) = self.scoped(session)? {
            return store.for_each_message_index(session, visit);
        }
        self.read(|conn| {
            let mut statement = conn
                .prepare("SELECT seq,message_id FROM messages WHERE session_id=?1 ORDER BY seq")?;
            let mut rows = statement.query([session])?;
            let mut count = 0;
            while let Some(row) = rows.next()? {
                let id: String = row.get(1)?;
                visit(row.get(0)?, &id)?;
                count += 1;
            }
            Ok(count)
        })
    }

    /// Stream an inclusive global sequence range, decoding one row at a time.
    /// The callback must not reenter this Store while its read lock is held.
    pub fn for_each_message_range(
        &self,
        session: &str,
        first: i64,
        last: i64,
        mut visit: impl FnMut(i64, Message) -> Result<()>,
    ) -> Result<usize> {
        self.for_each_message_range_while(session, first, last, |seq, message| {
            visit(seq, message)?;
            Ok(true)
        })
    }

    /// As the streaming range walker, stopping after the first false callback.
    pub fn for_each_message_range_while(
        &self,
        session: &str,
        first: i64,
        last: i64,
        mut visit: impl FnMut(i64, Message) -> Result<bool>,
    ) -> Result<usize> {
        if let Some(store) = self.scoped(session)? {
            return store.for_each_message_range_while(session, first, last, visit);
        }
        self.read(|conn| {
            let mut statement = conn.prepare("SELECT seq,data FROM messages WHERE session_id=?1 AND seq BETWEEN ?2 AND ?3 ORDER BY seq")?;
            let mut rows = statement.query(params![session,first,last])?;
            let mut count = 0;
            while let Some(row) = rows.next()? { let keep_going=visit(row.get(0)?, Self::message_from_data(conn, &row.get::<_,String>(1)?)?)?; count += 1; if !keep_going { break; } }
            Ok(count)
        })
    }

    /// A bounded page of an inclusive canonical sequence range.
    pub fn messages_range(
        &self,
        session: &str,
        first: i64,
        last: i64,
        limit: usize,
    ) -> Result<Vec<(i64, Message)>> {
        if let Some(store) = self.scoped(session)? {
            return store.messages_range(session, first, last, limit);
        }
        self.read(|conn| {
            let mut statement = conn.prepare("SELECT seq,data FROM messages WHERE session_id=?1 AND seq BETWEEN ?2 AND ?3 ORDER BY seq LIMIT ?4")?;
            let mut rows = statement.query(params![session,first,last,i64::try_from(limit)?])?;
            let mut messages = Vec::new();
            while let Some(row) = rows.next()? { messages.push((row.get(0)?,Self::message_from_data(conn,&row.get::<_,String>(1)?)?)); }
            Ok(messages)
        })
    }

    /// The newest bounded page strictly before a session-owned message ID.
    /// With no cursor, reads the newest page. Results are in ascending sequence
    /// order; an unknown or foreign cursor returns None, and a valid cursor at
    /// the beginning of the session returns Some with an empty page.
    pub fn messages_before(
        &self,
        session: &str,
        before_id: Option<&str>,
        limit: usize,
    ) -> Result<Option<Vec<(i64, Message)>>> {
        if let Some(store) = self.scoped(session)? {
            return store.messages_before(session, before_id, limit);
        }
        self.read(|conn| {
            let before_seq: Option<i64> = if let Some(id) = before_id {
                let seq = conn
                    .query_row(
                        "SELECT seq FROM messages WHERE session_id=?1 AND message_id=?2",
                        params![session, id],
                        |row| row.get(0),
                    )
                    .optional()?;
                let Some(seq) = seq else {
                    return Ok(None);
                };
                Some(seq)
            } else {
                None
            };
            let mut statement = conn.prepare(if before_seq.is_some() {
                "SELECT seq,data FROM messages WHERE session_id=?1 AND seq<?2 ORDER BY seq DESC LIMIT ?3"
            } else {
                "SELECT seq,data FROM messages WHERE session_id=?1 ORDER BY seq DESC LIMIT ?2"
            })?;
            let limit = i64::try_from(limit)?;
            let mut rows = if let Some(seq) = before_seq {
                statement.query(params![session, seq, limit])?
            } else {
                statement.query(params![session, limit])?
            };
            let mut messages = Vec::new();
            while let Some(row) = rows.next()? {
                messages.push((
                    row.get(0)?,
                    Self::message_from_data(conn, &row.get::<_, String>(1)?)?,
                ));
            }
            messages.reverse();
            Ok(Some(messages))
        })
    }

    /// Compress only old, compacted rows that are absent from the current view.
    pub fn compress_cold_messages(&self, session: &str) -> Result<usize> {
        let fallback = self
            .0
            .layout
            .as_ref()
            .map(|layout| layout.options())
            .or_else(|| {
                self.host()
                    .and_then(|host| host.0.layout.as_ref().map(|layout| layout.options()))
            })
            .unwrap_or_default();
        let options = self
            .session_storage_policy(session)?
            .map_or(fallback, |policy| policy.store);
        self.compress_cold_messages_with_options(session, options)
    }

    /// Explicit thresholds for offline compaction or a measured one-time pass.
    pub fn compress_cold_messages_with_options(
        &self,
        session: &str,
        options: StoreOptions,
    ) -> Result<usize> {
        let view = if let Some(host) = self.host() {
            host.cold_view_data(session)?
        } else {
            self.cold_view_data(session)?
        };
        if let Some(store) = self.scoped(session)? {
            return store.compress_cold_rows(session, options, view.as_deref());
        }
        self.compress_cold_rows(session, options, view.as_deref())
    }

    fn cold_view_data(&self, session: &str) -> Result<Option<String>> {
        self.read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT data FROM context_views WHERE session_id=?1",
                    [session],
                    |row| row.get(0),
                )
                .optional()?)
        })
    }

    fn compress_cold_rows(
        &self,
        session: &str,
        options: StoreOptions,
        view: Option<&str>,
    ) -> Result<usize> {
        let view_value = view
            .map(serde_json::from_str::<serde_json::Value>)
            .transpose()?;
        if view_value
            .as_ref()
            .is_some_and(|view| !view["frontier"].is_i64())
        {
            return Ok(0);
        }
        let plan = self.read(|conn| {
            let Some(frontier)=latest_frontier(conn,session)? else {return Ok(None);};
            let serving_frontier=view_value.as_ref().and_then(|view|view["frontier"].as_i64()).unwrap_or(frontier);
            let tail:Option<i64>=if options.cold_tail_messages==0 {Some(i64::MAX)} else {conn.query_row("SELECT seq FROM messages WHERE session_id=?1 ORDER BY seq DESC LIMIT 1 OFFSET ?2",params![session,options.cold_tail_messages-1],|row|row.get(0)).optional()?};
            // An undersized history lies entirely inside the protected hot tail.
            let Some(tail)=tail else {return Ok(None);};
            let through=frontier.min(serving_frontier).min(tail.saturating_sub(1));
            let policy=compression_policy_signature(conn,session,frontier,view_value.as_ref(),options)?;
            let completed:Option<String>=conn.query_row("SELECT value FROM metadata WHERE key=?1",[format!("cold_compression_pass:{session}")],|row|row.get(0)).optional()?;
            // Older receipts transparently take one complete pass. A
            // stable policy with a dictionary needs only newly cold sequences.
            let completed=completed.and_then(|receipt|serde_json::from_str::<CompressionPass>(&receipt).ok());
            let has_dictionary:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM message_dictionaries)",[],|row|row.get(0))?;
            let established_large=completed.as_ref().is_some_and(|pass|pass.minimum_messages==options.cold_min_messages && pass.minimum_bytes==options.cold_min_bytes);
            if completed.as_ref().is_some_and(|pass|pass.policy==policy && pass.through==through && pass.has_dictionary==has_dictionary) {return Ok(None);}
            let first=completed.as_ref().filter(|pass|pass.policy==policy && pass.through<through && pass.has_dictionary && has_dictionary).map_or(i64::MIN,|pass|pass.through.saturating_add(1));
            // A prior pass already met these thresholds. Appending immutable
            // records cannot shrink canonical history; avoid rescanning sizes.
            if !established_large {
                let count:i64=conn.query_row("SELECT COUNT(*) FROM messages WHERE session_id=?1",[session],|row|row.get(0))?;
                if count<i64::from(options.cold_min_messages) {
                    let bytes:i64=conn.query_row("SELECT COALESCE(SUM(length(data)),0) FROM messages WHERE session_id=?1",[session],|row|row.get(0))?;
                    if (bytes as u64)<options.cold_min_bytes {return Ok(None);}
                }
            }
            Ok(Some((first,through,policy)))
        })?;
        let Some((first, through, policy)) = plan else {
            return Ok(0);
        };
        let compressed = self.write(|conn| {
            initialize(conn)?;
            let mut has_dictionary=false;
            let count=(|| -> Result<usize> {
            let mut protected = HashSet::new();
            let mut ranges = Vec::new();
            if let Some(view) = view {
                let value: serde_json::Value = serde_json::from_str(view)?;
                // A fully raw serving view owns every row.
                if let Some(refs) = value.get("prefix_refs") {
                    let pointers: Vec<PrefixPointer> = serde_json::from_value(refs.clone())?;
                    for pointer in pointers { match pointer {
                        PrefixPointer::Messages {first,last} => ranges.push((first,last)),
                        PrefixPointer::Rendered {base_seq,content,fields,..} => {
                            protected.extend(base_seq);
                            for pointer in content.iter().chain(fields.values().flatten()) { if let TextPointer::Source {table,id,..} = pointer && table == "messages" { protected.insert(id.parse::<i64>()?); } }
                        }
                    }}
                } else if let Some(prefix) = value["prefix"].as_array() { for message in prefix { if let Some(id) = message["id"].as_str() { protected.insert(Self::session_message_seq(conn,session,id)?); } } }
            }
            let eligible = |seq: i64| !protected.contains(&seq) && !ranges.iter().any(|(first,last)|seq>=*first && seq<=*last);
            let existing: Option<String> = conn.query_row("SELECT hash FROM message_dictionaries LIMIT 1",[],|row|row.get(0)).optional()?;
            let (hash,dictionary) = if let Some(hash) = existing { let dictionary=pin_dictionary(conn,&self.0.dictionary_pins,&hash)?; (hash,dictionary) } else {
                let mut samples = Vec::new();
                let mut sample_bytes = 0;
                let mut statement = conn.prepare("SELECT seq,data FROM messages WHERE session_id=?1 AND seq<=?2 ORDER BY seq")?;
                let mut rows = statement.query(params![session,through])?;
                while let Some(row)=rows.next()? {
                    let seq = row.get(0)?; let data: String=row.get(1)?;
                    if eligible(seq) && data.len()>=options.cold_row_min_bytes as usize && data.len()<=SAMPLE_BYTES {
                        if sample_bytes + data.len() > SAMPLE_BYTES { continue; }
                        sample_bytes += data.len(); samples.push(data.into_bytes());
                    }
                    if sample_bytes>=SAMPLE_BYTES || SAMPLE_BYTES-sample_bytes<options.cold_row_min_bytes as usize { break; }
                }
                if samples.len()<8 || sample_bytes<1024 { return Ok(0); }
                let Ok(dictionary) = zstd::dict::from_samples(&samples,DICTIONARY_BYTES.min(sample_bytes/2)) else { return Ok(0); };
                let hash = format!("{:x}",Sha256::digest(&dictionary));
                conn.execute("INSERT INTO message_dictionaries VALUES(?1,?2)",params![hash,dictionary])?;
                let dictionary=pin_dictionary(conn,&self.0.dictionary_pins,&hash)?;
                (hash,dictionary)
            };
            has_dictionary=true;
            let mut compressor = zstd::bulk::Compressor::with_dictionary(3,&dictionary.bytes)?;
            let mut count = 0;
            let mut statement = conn.prepare("SELECT seq,data FROM messages WHERE session_id=?1 AND seq BETWEEN ?2 AND ?3 ORDER BY seq")?;
            let mut rows = statement.query(params![session,first,through])?;
            while let Some(row)=rows.next()? {
                let seq = row.get(0)?; let data: String=row.get(1)?;
                if !eligible(seq) || data.len()<options.cold_row_min_bytes as usize || data.len()>64*1024*1024 || serde_json::from_str::<serde_json::Value>(&data)?["_row_codec"].is_string() { continue; }
                let compressed = compressor.compress(data.as_bytes())?;
                let encoded=serde_json::json!({"_row_codec":CODEC,"dictionary":hash,"raw_bytes":data.len(),"sha256":format!("{:x}",Sha256::digest(data.as_bytes())),"data":base64::engine::general_purpose::STANDARD.encode(compressed)}).to_string();
                if encoded.len()<data.len() { conn.execute("UPDATE messages SET data=?1 WHERE seq=?2",params![encoded,seq])?; count += 1; }
            }
            Ok(count)
            })()?;
            // Errors roll the receipt and row changes back together; completed
            // training failures and no-work outcomes keep their exact policy.
            let receipt=CompressionPass {policy,through,has_dictionary,minimum_messages:options.cold_min_messages,minimum_bytes:options.cold_min_bytes};
            conn.execute("INSERT OR REPLACE INTO metadata VALUES(?1,?2)",params![format!("cold_compression_pass:{session}"),serde_json::to_string(&receipt)?])?;
            Ok(count)
        })?;
        if compressed > 0 {
            self.read(|conn| {
                let mut statement = conn.prepare("PRAGMA incremental_vacuum(4096)")?;
                let mut rows = statement.query([])?;
                while rows.next()?.is_some() {}
                Ok(())
            })?;
        }
        Ok(compressed)
    }
}

pub(super) fn archive_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.zst", path.display()))
}
pub(super) fn staged_path(path: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}.{suffix}", path.display()))
}

fn hash_reader(mut reader: impl Read) -> Result<(Vec<u8>, u64)> {
    let mut hash = Sha256::new();
    let mut bytes = 0;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
        bytes += read as u64;
    }
    Ok((hash.finalize().to_vec(), bytes))
}
pub(super) fn verify_archive(path: &Path, archive: &Path) -> Result<()> {
    let original = hash_reader(std::fs::File::open(path)?)?;
    let packed = hash_reader(zstd::stream::read::Decoder::new(std::fs::File::open(
        archive,
    )?)?)?;
    anyhow::ensure!(original == packed, "Whole-chat archive verification failed");
    Ok(())
}
pub(super) fn pack(path: &Path) -> Result<()> {
    let final_path = archive_path(path);
    let part = staged_path(&final_path, "part");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&part)?;
    let mut encoder = zstd::stream::write::Encoder::new(file, 9)?;
    encoder.include_checksum(true)?;
    std::io::copy(&mut std::fs::File::open(path)?, &mut encoder)?;
    encoder.finish()?.sync_all()?;
    verify_archive(path, &part)?;
    if final_path.exists() {
        std::fs::remove_file(&final_path)?;
    }
    std::fs::rename(&part, &final_path)?;
    sync_parent(path)?;
    Ok(())
}
pub(super) fn unpack(archive: &Path, target: &Path) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(target)?;
    let mut decoder = zstd::stream::read::Decoder::new(std::fs::File::open(archive)?)?;
    let mut writer = std::io::BufWriter::new(file);
    std::io::copy(&mut decoder, &mut writer)?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(())
}
pub(super) fn sync_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    std::fs::File::open(path.parent().context("Missing chat directory")?)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
