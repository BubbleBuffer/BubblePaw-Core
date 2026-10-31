//! Private fixture acceptance: emit only counts, sizes, hashes and timings.
use anyhow::{Context, Result, ensure};
use bubblepaw_core::transcript::{normalized_derived, normalized_message};
use bubblepaw_core::{ImportFormat, ImportOptions, ModelRequest, Store, StoreOptions, import_file};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{io::Write, path::Path, time::Instant};

#[derive(Default)]
struct ExportProof {
    line: Vec<u8>,
    messages: Sha256,
    derived: Sha256,
    whole: Sha256,
    message_count: u64,
    derived_count: u64,
}
impl Write for ExportProof {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.whole.update(bytes);
        for byte in bytes {
            if *byte != b'\n' {
                self.line.push(*byte);
                continue;
            }
            let record: Value =
                serde_json::from_slice(&self.line).map_err(std::io::Error::other)?;
            let (value, digest, count) = if let Some(message) = record.get("message") {
                let message =
                    serde_json::from_value(message.clone()).map_err(std::io::Error::other)?;
                (
                    normalized_message(&message),
                    &mut self.messages,
                    &mut self.message_count,
                )
            } else if let Some(derived) = record.get("derived") {
                let derived =
                    serde_json::from_value(derived.clone()).map_err(std::io::Error::other)?;
                (
                    normalized_derived(&derived),
                    &mut self.derived,
                    &mut self.derived_count,
                )
            } else {
                self.line.clear();
                continue;
            };
            digest.update(serde_json::to_vec(&value).map_err(std::io::Error::other)?);
            digest.update(b"\n");
            *count += 1;
            self.line.clear();
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn hex(hash: &Sha256) -> String {
    bubblepaw_core::hex_lower(hash.clone().finalize())
}
fn export(store: &Store, session: &str) -> Result<ExportProof> {
    let mut proof = ExportProof::default();
    store.export_jsonl(session, &mut proof)?;
    ensure!(
        proof.line.is_empty(),
        "Export ended with an incomplete record"
    );
    Ok(proof)
}
fn request(store: &Store, session: &str) -> Result<(Vec<u8>, f64)> {
    let started = Instant::now();
    let request = ModelRequest {
        model: "measurement/no-provider".into(),
        system_prompt: "Continue this imported chat using the source's retained summary.".into(),
        session_id: session.into(),
        messages: store.serving_messages(session)?,
        ..Default::default()
    };
    let bytes = serde_json::to_vec(&request)?;
    Ok((bytes, started.elapsed().as_secs_f64()))
}
fn hits(store: &Store, session: &str) -> Result<(Vec<(i64, String)>, f64)> {
    let started = Instant::now();
    let hits = store.search_messages("error", Some(session), 50)?;
    Ok((
        hits.into_iter()
            .map(|hit| (hit.seq, hit.message_id))
            .collect(),
        started.elapsed().as_secs_f64(),
    ))
}
fn index_proof(store: &Store, directory: &str) -> Result<(u64, String)> {
    store.flush_serving()?;
    let connection = rusqlite::Connection::open_with_flags(
        Path::new(directory).join("serving.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let mut statement = connection.prepare("SELECT r.seq,r.message_id FROM message_search JOIN search_rows r ON r.id=message_search.rowid WHERE message_search MATCH 'error' ORDER BY r.seq,r.message_id")?;
    let mut rows = statement.query([])?;
    let mut digest = Sha256::new();
    let mut count = 0;
    while let Some(row) = rows.next()? {
        let seq: i64 = row.get(0)?;
        let id: String = row.get(1)?;
        digest.update(seq.to_le_bytes());
        digest.update((id.len() as u64).to_le_bytes());
        digest.update(id.as_bytes());
        count += 1;
    }
    Ok((count, hex(&digest)))
}
fn size(path: &Path) -> Result<u64> {
    Ok(std::fs::metadata(path)?.len())
}
fn peak_rss_kib() -> Option<u64> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find(|line| line.starts_with("VmHWM:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let format = match args.next().as_deref() {
        Some("codex") => ImportFormat::Codex,
        Some("claude-code") => ImportFormat::Claude,
        _ => anyhow::bail!(
            "usage: storage_phase_c_measure <codex|claude-code> <source> <new-private-output-directory>"
        ),
    };
    let source = args.next().context("source path required")?;
    let directory = args
        .next()
        .context("new private output directory required")?;
    ensure!(
        !Path::new(&directory).exists(),
        "Measurement output already exists"
    );
    let store = Store::open_with_options(
        &directory,
        StoreOptions {
            archive_after_days: 0,
            cold_min_messages: u32::MAX,
            cold_min_bytes: u64::MAX,
            ..Default::default()
        },
    )?;
    let started = Instant::now();
    let imported = import_file(&store, &source, format, &ImportOptions::default())?;
    let import_seconds = started.elapsed().as_secs_f64();
    let import_peak_rss_kib = peak_rss_kib();
    let session = &imported.root_session_id;
    store.idle_checkpoint()?;
    let path = store.chat_path(session)?.context("Missing imported chat")?;
    let plain_bytes = size(&path)?;
    let plain_export = export(&store, session)?;
    let receipt = imported.sources.first().context("Missing import receipt")?;
    ensure!(
        hex(&plain_export.messages) == receipt.normalized_sha256
            && hex(&plain_export.derived) == receipt.normalized_derived_sha256,
        "Export does not match the imported normalized content"
    );
    let (plain_request, request_plain_seconds) = request(&store, session)?;
    while !store.reindex_search(Some(session), 512)?.complete {}
    let (plain_hits, query_plain_seconds) = hits(&store, session)?;
    ensure!(!plain_hits.is_empty(), "Fixture query produced no hits");
    let all_hits = index_proof(&store, &directory)?;
    store.flush_serving()?;
    let serving = rusqlite::Connection::open_with_flags(
        Path::new(&directory).join("serving.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let index_bytes: i64 = serving.query_row("SELECT COALESCE(SUM(pgsize),0) FROM dbstat WHERE name LIKE 'message_search%' OR name LIKE 'search_%'", [], |row| row.get(0))?;
    drop(serving);
    let started = Instant::now();
    let compressed_rows =
        store.compress_cold_messages_with_options(session, StoreOptions::default())?;
    let compression_seconds = started.elapsed().as_secs_f64();
    store.idle_checkpoint()?;
    let row_compressed_bytes = size(&path)?;
    let (compressed_request, request_compressed_seconds) = request(&store, session)?;
    ensure!(
        plain_request == compressed_request,
        "Row compression changed provider request bytes"
    );
    let (compressed_hits, query_compressed_seconds) = hits(&store, session)?;
    ensure!(
        plain_hits == compressed_hits,
        "Row compression changed search hits"
    );
    ensure!(
        all_hits == index_proof(&store, &directory)?,
        "Row compression changed the full indexed hit set"
    );
    let started = Instant::now();
    let mut scan_hash = Sha256::new();
    let mut text_bytes = 0u64;
    let full_history_messages =
        store.for_each_message_range(session, 0, i64::MAX, |_, message| {
            text_bytes += message.content.len() as u64;
            for call in &message.tool_calls {
                text_bytes +=
                    call.name.len() as u64 + serde_json::to_vec(&call.arguments)?.len() as u64;
            }
            scan_hash.update(serde_json::to_vec(&normalized_message(&message))?);
            scan_hash.update(b"\n");
            Ok(())
        })?;
    let full_history_seconds = started.elapsed().as_secs_f64();
    ensure!(
        hex(&scan_hash) == receipt.normalized_sha256,
        "Compressed full history differs from import"
    );
    ensure!(store.archive_chat(session)?, "Chat could not be archived");
    let archived_path = path.with_extension("db.zst");
    let archived_bytes = size(&archived_path)?;
    let mut archived_export = ExportProof::default();
    Store::export_data_dir_jsonl(&directory, session, &mut archived_export)?;
    ensure!(
        hex(&plain_export.whole) == hex(&archived_export.whole),
        "Archived export changed content"
    );
    ensure!(
        !path.exists() && archived_path.exists(),
        "Read-only export restored an archive"
    );
    let started = Instant::now();
    store.session(session)?;
    let archived_open_seconds = started.elapsed().as_secs_f64();
    let (archived_hits, query_archived_seconds) = hits(&store, session)?;
    ensure!(plain_hits == archived_hits, "Archiving changed search hits");
    ensure!(
        all_hits == index_proof(&store, &directory)?,
        "Archiving changed the full indexed hit set"
    );
    let (archived_request, request_archived_seconds) = request(&store, session)?;
    ensure!(
        plain_request == archived_request,
        "Archive restore changed provider request bytes"
    );
    ensure!(
        store.archive_chat(session)?,
        "Chat could not be archived before the index wipe"
    );
    store.serving_wipe()?;
    ensure!(
        !path.exists() && archived_path.exists(),
        "Serving wipe eagerly unpacked an archived chat"
    );
    let started = Instant::now();
    while !store.reindex_search(Some(session), 512)?.complete {}
    let rebuild_seconds = started.elapsed().as_secs_f64();
    let (rebuilt_hits, query_rebuilt_seconds) = hits(&store, session)?;
    ensure!(
        plain_hits == rebuilt_hits,
        "Serving wipe/rebuild changed search hits"
    );
    ensure!(
        all_hits == index_proof(&store, &directory)?,
        "Serving wipe changed the full indexed hit set"
    );
    let (rebuilt_request, request_rebuilt_seconds) = request(&store, session)?;
    ensure!(
        plain_request == rebuilt_request,
        "Serving wipe changed imported request bytes"
    );
    store.delete_session(session)?;
    ensure!(
        store.search_messages("error", None, 50)?.is_empty(),
        "Deleted chat remains searchable"
    );
    ensure!(
        index_proof(&store, &directory)?.0 == 0,
        "Deleted chat left indexed hit references"
    );
    println!(
        "{}",
        json!({"fixture":Path::new(&source).file_name().and_then(|name| name.to_str()),"source_bytes":imported.stats.source_bytes,
        "import_stats":imported.stats,"import_seconds":import_seconds,"import_peak_rss_kib":import_peak_rss_kib,
        "plain_bytes":plain_bytes,"row_compressed_bytes":row_compressed_bytes,"archived_bytes":archived_bytes,
        "compressed_rows":compressed_rows,"compression_seconds":compression_seconds,"archived_open_seconds":archived_open_seconds,
        "full_history_messages":full_history_messages,"full_history_stream_seconds":full_history_seconds,
        "request_plain_seconds":request_plain_seconds,"request_compressed_seconds":request_compressed_seconds,
        "request_archived_seconds":request_archived_seconds,"request_rebuilt_seconds":request_rebuilt_seconds,
        "request_bytes":plain_request.len(),"index_bytes":index_bytes,"indexed_text_bytes":text_bytes,
        "index_to_text_ratio":index_bytes as f64 / text_bytes.max(1) as f64,
        "query_plain_seconds":query_plain_seconds,"query_compressed_seconds":query_compressed_seconds,
        "query_archived_seconds":query_archived_seconds,"query_rebuilt_seconds":query_rebuilt_seconds,
        "search_hits_checked":plain_hits.len(),"rebuild_seconds":rebuild_seconds,
        "all_search_hits_checked":all_hits.0,
        "normalized_messages_sha256":hex(&plain_export.messages),"normalized_derived_sha256":hex(&plain_export.derived),
        "export_messages":plain_export.message_count,"export_derived":plain_export.derived_count,"checks":"passed",
        "measurement_basis":"Linux release; no provider calls; full history is a bounded decoded scan; request is source compaction summary plus tail with no tools"})
    );
    Ok(())
}
