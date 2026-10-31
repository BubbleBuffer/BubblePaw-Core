//! Runner-owned checkpoints for edit and write. Core checkpoint records own
//! compressed, content-addressed preimages; the core store needs no file-specific schema.
use anyhow::{Context, Result, anyhow, ensure};
use bubblepaw_core::{
    CHECKPOINT_PREFIX, ChatOpenHook, CheckpointRecord, Message, MessageOrigin, Role, Store,
    ToolContext, is_typed_user,
};
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::tools::WorkspaceTools;

const PREFIX: &str = CHECKPOINT_PREFIX;
const CHECKPOINT_LIMIT: usize = 10 * 1024 * 1024;
/// Path and reason of the unrestorable record the sweep writes for a session with a
/// legacy row whose metadata cannot be read. The row's own path is unknown.
const UNREADABLE_PATH: &str = "a legacy checkpoint with unreadable metadata";
const UNREADABLE_REASON: &str = "its metadata cannot be read, so its turn is unknown";
const LIMITATION: &str = "Only this session's checkpointed file changes are restored. Exec, MCP, other sessions and outside edits are not rewound; forks start their own checkpoint ownership.";

#[derive(Debug, Serialize, Deserialize)]
struct Checkpoint {
    workspace: PathBuf,
    path: String,
    invocation_id: String,
    boundary_message_id: Option<String>,
    boundary_seq: i64,
    patch_index: u64,
    pre_hash: Option<String>,
    post_hash: Option<String>,
    barrier: Option<String>,
    /// Set on a record the upgrade sweep migrated without its preimage. Its turn
    /// cannot be restored, so rewind refuses it. It is never written by a patch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unrestorable: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CheckpointSummary {
    pub stored: usize,
    pub skipped: Vec<CheckpointSkip>,
}

#[derive(Debug, Serialize)]
pub struct CheckpointSkip {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct RewindConflict {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct RewindResult {
    pub session_id: String,
    pub restored: Vec<String>,
    pub deleted: Vec<String>,
    pub recreated: Vec<String>,
    pub conflicts: Vec<RewindConflict>,
    pub limitation: String,
}

/// Recognize only the slash command itself, retaining an empty argument so the
/// host can explain a missing target without submitting it to the model.
pub fn rewind_command(input: &str) -> Option<&str> {
    let input = input.trim();
    if input == "/rewind" {
        return Some("");
    }
    input
        .strip_prefix("/rewind")
        .filter(|rest| rest.starts_with(char::is_whitespace))
        .map(str::trim)
}

pub(crate) fn content_hash(bytes: Option<&[u8]>) -> Option<String> {
    bytes.map(bubblepaw_core::fingerprint)
}

fn checkpoints(store: &Store, session: &str) -> Result<Vec<(String, Checkpoint)>> {
    let mut result = Vec::new();
    let mut after = None;
    loop {
        let page = store.checkpoint_records_page(session, after.as_deref(), 256)?;
        if page.is_empty() {
            break;
        }
        after = page.last().map(|(id, _)| id.clone());
        for (id, metadata) in page {
            let checkpoint: Checkpoint =
                serde_json::from_str(&metadata).context("invalid file checkpoint metadata")?;
            // Earlier Windows patches persisted native separators. Group those
            // records with new slash-separated paths before checking the chain.
            // Unix backslashes are literal filename characters and stay intact.
            #[cfg(windows)]
            let checkpoint = Checkpoint {
                path: checkpoint.path.replace('\\', "/"),
                ..checkpoint
            };
            result.push((id, checkpoint));
        }
    }
    Ok(result)
}

/// Persist every intent before allowing any target write. A failed metadata
/// commit therefore aborts the patch. Oversized files still get durable barriers.
pub(crate) fn checkpoint_changes<'a>(
    store: &Store,
    workspace: &Path,
    context: &ToolContext,
    changes: impl Iterator<Item = (&'a str, Option<&'a [u8]>, Option<&'a [u8]>)>,
) -> Result<CheckpointSummary> {
    // Unreadable existing records abort before any target write. Pruning reads only
    // boundary sequences, so this is the one place that parses the stored metadata.
    checkpoints(store, &context.session_id)?;
    // Numbering continues from the session's high-water mark, which pruning never
    // lowers, so an index is never reused. The store rejects a write below it.
    let previous_index = store.checkpoint_high_water(&context.session_id)?;
    let patch_index = previous_index
        .checked_add(1)
        .context("checkpoint index exhausted")?;
    // Nested js calls have their own invocation IDs. The most recent canonical
    // message is the durable owning assistant/cell boundary in either case.
    let boundary = store
        .messages_before(&context.session_id, None, 1)?
        .unwrap_or_default()
        .pop();
    let boundary_seq = boundary.as_ref().map_or(0, |(seq, _)| *seq);
    let boundary_message_id = boundary.map(|(_, message)| message.id);
    let mut summary = CheckpointSummary {
        stored: 0,
        skipped: Vec::new(),
    };
    let mut planned = false;
    for (index, (path, previous, content)) in changes.enumerate() {
        if previous == content {
            continue;
        }
        let barrier = (previous
            .map_or(0, <[u8]>::len)
            .max(content.map_or(0, <[u8]>::len))
            > CHECKPOINT_LIMIT)
            .then(|| {
                "file exceeds 10 MiB checkpoint limit; rewind cannot cross this change".to_owned()
            });
        let checkpoint = Checkpoint {
            workspace: workspace.to_owned(),
            path: path.to_owned(),
            invocation_id: context.invocation_id.clone(),
            boundary_message_id: boundary_message_id.clone(),
            boundary_seq,
            patch_index,
            pre_hash: content_hash(previous),
            post_hash: content_hash(content),
            barrier,
            unrestorable: None,
        };
        let id = format!("{PREFIX}{patch_index:020}/{index:06}");
        let bytes = if checkpoint.barrier.is_some() {
            &[][..]
        } else {
            previous.unwrap_or_default()
        };
        let metadata = serde_json::to_string(&checkpoint)?;
        // The prune bounds ride the first checkpoint write of this call. The store
        // applies them inside that write's transaction, so pruning adds no
        // transaction of its own.
        let prunes = if planned {
            Vec::new()
        } else {
            planned = true;
            prune_plan(store, &context.session_id)?
        };
        store.write_checkpoint(
            &context.session_id,
            CheckpointRecord {
                id: &id,
                patch_index,
                boundary_seq,
                metadata: &metadata,
            },
            bytes,
            &prunes,
        )?;
        if let Some(reason) = checkpoint.barrier {
            summary.skipped.push(CheckpointSkip {
                path: path.to_owned(),
                reason,
            });
        } else {
            summary.stored += 1;
        }
    }
    Ok(summary)
}

/// The sweep marker and its rule version. Both are set in the sweep's transaction.
const PRUNE_MARKER: &str = "rewind_checkpoint_prune";
const PRUNE_RULE: &str = "v2";
/// Hook key of the sweep. The core registers each key once, so every host
/// construction may install it again.
const SWEEP_HOOK: &str = "rewind.checkpoint-sweep";

/// Register the one-time checkpoint sweep. A chat store runs it the first time
/// this process opens it, outside any checkpoint write, and runs it now on the
/// chats already open (the directory store opens active chats at startup).
/// Every agent host calls this on its store when it is constructed.
pub fn install_checkpoint_sweep(store: &Store) -> Result<()> {
    let hook: ChatOpenHook = Arc::new(sweep_chat);
    // A chat whose sweep fails stays unmigrated and refuses rewind. The other open
    // chats are still swept.
    for failure in store.on_chat_open(SWEEP_HOOK, hook)? {
        tracing::warn!(
            chat = %failure.chat.display(),
            error = %format!("{:#}", failure.error),
            "rewind checkpoint sweep failed; this chat refuses rewind until it succeeds"
        );
    }
    Ok(())
}

/// Delete every legacy checkpoint record of one opened chat, once, and set the
/// marker in the same transaction. No retention applies: legacy records are
/// never restorable under the current layout. Their blobs are not reclaimed
/// here; they go with the chat. The sweep only ever opens the chat it is given,
/// so archived chats stay archived.
fn sweep_chat(chat: &Store) -> Result<()> {
    let sessions = chat.chat_session_ids()?;
    let Some(first) = sessions.first() else {
        // A chat with no sessions has no legacy rows. Mark it now: it is cached, so
        // its sweep never runs again, and a session created in it later must rewind.
        return chat.set_chat_metadata(PRUNE_MARKER, PRUNE_RULE);
    };
    if chat.chat_metadata(first, PRUNE_MARKER)?.as_deref() == Some(PRUNE_RULE) {
        return Ok(());
    }
    // Legacy rows at or above each session's keep bound are copied into the current
    // tables. A session with fewer than two turns keeps all of its rows.
    let mut keep = Vec::new();
    let mut unreadable = Vec::new();
    for session in &sessions {
        if let Some(from) = keep_from(chat, session)? {
            keep.push((session.clone(), from));
        }
        // An unreadable legacy row's turn is unknown, so its record sits at the latest
        // turn start. Rewinding that turn then refuses.
        if let Some((seq, message_id)) = turn_starts(chat, session, 1)?.pop() {
            let checkpoint = Checkpoint {
                workspace: PathBuf::new(),
                path: UNREADABLE_PATH.to_owned(),
                invocation_id: String::new(),
                boundary_message_id: Some(message_id),
                boundary_seq: seq,
                patch_index: 0,
                pre_hash: None,
                post_hash: None,
                barrier: None,
                unrestorable: Some(UNREADABLE_REASON.to_owned()),
            };
            unreadable.push((session.clone(), seq, serde_json::to_string(&checkpoint)?));
        }
    }
    let skipped =
        chat.sweep_legacy_checkpoints(first, (PRUNE_MARKER, PRUNE_RULE), &keep, &unreadable)?;
    if !skipped.is_empty() {
        let chat_path = chat.chat_path(first)?.unwrap_or_default();
        for row in skipped {
            tracing::warn!(
                chat = %chat_path.display(),
                checkpoint = %row.id,
                session = %row.session_id,
                reason = %row.reason,
                left = row.left,
                "legacy checkpoint could not be restored; left=true keeps it in attachments"
            );
        }
    }
    Ok(())
}

/// A turn starts at a typed user row (run, steer or followup) or at a child-task
/// instruction. Host notices and other generated rows never start a turn.
fn is_turn_start(message: &Message) -> bool {
    message.role == Role::User
        && match message.origin {
            Some(MessageOrigin::Typed | MessageOrigin::ChildTask) => true,
            Some(_) => false,
            None => is_typed_user(message),
        }
}

/// The newest `count` turn starts of a session, newest first, as (sequence, ID).
/// Pages backwards from the newest message, so the walk stops at those turns.
fn turn_starts(store: &Store, session_id: &str, count: usize) -> Result<Vec<(i64, String)>> {
    let mut found = Vec::new();
    let mut before: Option<String> = None;
    while found.len() < count {
        let page = store
            .messages_before(session_id, before.as_deref(), 256)?
            .context("rewind message cursor is not in this session")?;
        let Some((_, oldest)) = page.first() else {
            break;
        };
        before = Some(oldest.id.clone());
        for (seq, message) in page.iter().rev() {
            if found.len() == count {
                break;
            }
            if is_turn_start(message) {
                found.push((*seq, message.id.clone()));
            }
        }
    }
    Ok(found)
}

/// The keep bound of one session: the sequence of its previous turn's start. A
/// checkpoint whose boundary sequence is below it belongs to an older turn, which
/// the rewind target can no longer reach. `None` when the session has fewer than
/// two turns, so nothing is stale. Read only.
fn keep_from(store: &Store, session_id: &str) -> Result<Option<i64>> {
    Ok(turn_starts(store, session_id, 2)?
        .get(1)
        .map(|(seq, _)| *seq))
}

/// Prune bounds of every session in the writing session's chat, applied inside
/// the checkpoint write's transaction. Sessions with no records or fewer than two
/// turns are left out. Read only. Legacy records are not pruned here: the
/// one-time sweep handles them when the chat opens.
fn prune_plan(store: &Store, session_id: &str) -> Result<Vec<(String, i64)>> {
    let mut prunes = Vec::new();
    for session in store.chat_sessions(session_id)? {
        if store.checkpoint_records_page(&session, None, 1)?.is_empty() {
            continue;
        }
        if let Some(from) = keep_from(store, &session)? {
            prunes.push((session, from));
        }
    }
    Ok(prunes)
}

/// Checkpoint records to restore, per path, in patch order. Built under the
/// source's admission guard, so the target and the records are a consistent view.
type Paths = BTreeMap<String, Vec<(String, Checkpoint)>>;

fn rewind_plan(
    store: &Store,
    workspace: &WorkspaceTools,
    session_id: &str,
    message_id: &str,
) -> Result<Paths> {
    let cutoff = store
        .message_seq(session_id, message_id)
        .context("rewind target message is not in this session")?;
    let latest = turn_starts(store, session_id, 1)?.pop().map(|(_, id)| id);
    ensure!(
        latest.as_deref() == Some(message_id),
        "only the last message can be rewound: rewind accepts only the latest user message of this session, and {message_id} is not it"
    );
    let mut paths: Paths = BTreeMap::new();
    for (id, checkpoint) in checkpoints(store, session_id)? {
        let boundary = checkpoint
            .boundary_message_id
            .as_deref()
            .and_then(|message_id| store.message_seq(session_id, message_id).ok());
        ensure!(
            boundary == Some(checkpoint.boundary_seq),
            "checkpoint boundary no longer matches source history; refusing rewind"
        );
        if let Some(reason) = &checkpoint.unrestorable {
            // Refuse before any fork or write: a partial restore would report success.
            ensure!(
                checkpoint.boundary_seq < cutoff,
                "rewind refused: {} has an unrestorable checkpoint in the turn being rewound ({reason}); its earlier content cannot be restored",
                checkpoint.path
            );
            continue;
        }
        if checkpoint.boundary_seq >= cutoff {
            ensure!(
                checkpoint.workspace == workspace.root(),
                "checkpoint workspace {} differs from current workspace {}; refusing rewind",
                checkpoint.workspace.display(),
                workspace.root().display()
            );
            paths
                .entry(checkpoint.path.clone())
                .or_default()
                .push((id, checkpoint));
        }
    }
    Ok(paths)
}

/// Fork history through the target (inclusive), then restore this session's
/// later patch effects only where hashes prove an unbroken chain. Each restore
/// uses the patch tool's staged, per-file compare-and-swap commit.
///
/// The target check and the checkpoint plan run after the source's admission
/// guard is taken, so no turn can start between them and the fork. Core's fork
/// callback keeps that exclusion through all restores. The workspace mutation
/// guard also excludes concurrent runner patch writes; exec and other outside
/// processes remain subject to hash conflict checks.
pub fn rewind_session(
    store: &Store,
    workspace: impl AsRef<Path>,
    session_id: &str,
    message_id: &str,
) -> Result<RewindResult> {
    ensure!(
        !message_id.trim().is_empty(),
        "rewind requires a target message ID"
    );
    // A chat whose sweep has not finished still holds legacy rows, so its history is
    // incomplete. Refuse before any workspace or fork work.
    // An in-memory store has no legacy data, so it has nothing to upgrade.
    if store.chat_path(session_id)?.is_some() {
        ensure!(
            store.chat_metadata(session_id, PRUNE_MARKER)?.as_deref() == Some(PRUNE_RULE),
            "rewind refused: this chat's checkpoint upgrade has not run; reopen the chat so the sweep can finish, then retry"
        );
    }
    let workspace = crate::worktrees::workspace(store, session_id, workspace.as_ref())?;
    let workspace = WorkspaceTools::new(workspace)?;
    let _guard = workspace.mutation_guard()?;
    let source = store.session(session_id)?;
    let plan: RefCell<Option<Paths>> = RefCell::new(None);
    let mut result = RewindResult {
        session_id: String::new(),
        restored: Vec::new(),
        deleted: Vec::new(),
        recreated: Vec::new(),
        conflicts: Vec::new(),
        limitation: LIMITATION.to_owned(),
    };
    store.fork_at_admitted(
        session_id,
        message_id,
        format!("{} (rewind)", source.title),
        || {
            *plan.borrow_mut() = Some(rewind_plan(store, &workspace, session_id, message_id)?);
            Ok(())
        },
        |fork| {
            result.session_id.clone_from(&fork.id);
            // Todo is the only runner state with historical fork semantics. Questions
            // belong to their original waiter/session and are never copied.
            store.copy_host_state_at(session_id, message_id, &fork.id, "todo")?;
            let paths = plan.borrow_mut().take().context("rewind plan is missing")?;
            for (path, changes) in paths {
                let restore = (|| -> Result<Option<(bool, bool)>> {
                    for pair in changes.windows(2) {
                        ensure!(pair[0].1.post_hash == pair[1].1.pre_hash,
                            "checkpoint chain differs: outside edit between patches or unknown interrupted outcome");
                    }
                    let current = workspace.checkpoint_current_hash(&path)?;
                    for (_, checkpoint) in &changes {
                        if let Some(barrier) = &checkpoint.barrier { return Err(anyhow!(barrier.clone())); }
                    }
                    let (id, checkpoint) = changes.first().context("empty checkpoint chain")?;
                    ensure!(current == changes.last().context("empty checkpoint chain")?.1.post_hash,
                        "file differs from the last checkpoint postimage (outside edit or interrupted patch)");
                    if checkpoint.pre_hash == current { return Ok(None); }
                    // Preimages come only from checkpoint_records and checkpoint_blobs.
                    // Legacy attachment rows were swept when the chat opened.
                    let content = if checkpoint.pre_hash.is_some() {
                        let bytes = store.checkpoint_preimage(session_id, id)?.context("checkpoint preimage is missing")?;
                        ensure!(content_hash(Some(&bytes)) == checkpoint.pre_hash, "checkpoint preimage hash differs");
                        Some(bytes)
                    } else { None };
                    let deleted = content.is_none();
                    let recreated = current.is_none() && content.is_some();
                    workspace.restore_checkpoint(&path, current.as_deref(), content)?;
                    Ok(Some((deleted, recreated)))
                })();
                match restore {
                    Ok(Some((true, _))) => result.deleted.push(path),
                    Ok(Some((false, true))) => result.recreated.push(path),
                    Ok(Some((false, false))) => result.restored.push(path),
                    Ok(None) => {},
                    Err(error) => result.conflicts.push(RewindConflict { path, reason: format!("{error:#}") }),
                }
            }
            Ok(())
        },
    )?;
    Ok(result)
}
