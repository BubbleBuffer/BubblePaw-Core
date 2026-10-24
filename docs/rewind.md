# File checkpoints and rewind

Status: built 2026-10-15; scripted acceptance is recorded in
[verification](verification.md).

`edit` and `write` record each changed file's original bytes (or absence) and
the SHA-256 hash of its intended final bytes. Records belong to the calling
session and tool invocation. Existing checkpoints from the former patch tool,
including deletion and move records, remain valid. Unchanged files need no
checkpoint. The mutation surface was revised 2026-10-20; earlier verification
remains dated evidence about the former tool.

Preimages live in two core checkpoint tables, separate from `attachments` and from
images. `checkpoint_blobs(hash, raw_bytes, data)` holds each preimage, compressed with
zstd and keyed by its plain SHA-256, and `checkpoint_records(session_id, id, boundary_seq, metadata,
preimage_hash)` holds one row per changed file. Equal original bytes occupy one
`checkpoint_blobs` row within the chat file. Metadata is one JSON column and includes
the canonical workspace, relative path, invocation ID, canonical message boundary and
change order. Both tables are added with `CREATE TABLE IF NOT EXISTS`, so the schema
version does not change. Each changed file adds one checkpoint write transaction before
filesystem writes. If writing or cancellation interrupts a mutation, the durable
intent remains; rewind fails closed when the recorded chain cannot prove safety.
Checkpoint append reads only the last checkpoint key and latest message; it does
not reload the conversation or checkpoint inventory. Calling a mutation capability
without its dispatcher fails before mutation because no authoritative Store is
available for its checkpoint.

The checkpoint cap is 10 MiB per file. If either original or final content exceeds
that cap, `edit` or `write` can still apply within the separate 32 MiB file limit.
A durable barrier prevents rewind from crossing that change. Neither compact
mutation result exposes checkpoint metadata: `edit` returns `{path, replacements}`
and `write` returns `{path, bytes, created}`. Text reads retain their 8 MiB limit.

Deleting the owning chat or session deletes its `checkpoint_records` rows, then the
`checkpoint_blobs` rows that no remaining record names, in the same store transaction as
the ordinary session delete. Archiving keeps checkpoints and originals. A fork starts its
own checkpoint ownership; it does not acquire file changes made by its ancestors.

The first checkpoint write of each `edit` or `write` call also prunes, in the same
transaction (`write_checkpoint`). For each session of the chat, the transaction deletes
the records whose boundary sequence is below that session's keep bound, the sequence of
its previous turn's start, and then deletes each of their preimages that no remaining
record names. The bounds are read before the transaction, but the rows are judged inside
it: a record written after a bound was read is never removed by that bound, and nothing
is deleted by record ID. Sessions with fewer than two turns have no bound, so nothing in
them is stale. Pruning reads no reference text and scans no hex runs; the orphan check
uses the `checkpoint_records_preimage` index. Turn starts are the same rows the rewind
target uses. Pruning adds no transaction of its own.

High-water numbering: patch indexes come from a per-session high-water mark, stored in
the chat metadata under `checkpoint_high_water:<session>`. A write must be at or above
it, so an index is never reused: equal is allowed (several records from one runner call
share an index), below is refused. Pruning never lowers it. The sweep raises it to the
highest legacy patch index it can read, including rows it leaves in place.

Open order: a chat's open hooks run before the chat is published to the open cache, under
the same lock. No other caller can receive the chat until its migration has run. A hook
that fails on open fails that open and leaves the chat unpublished, so the next open
tries again.

Records from the earlier layout were checkpoint rows in `attachments`, with preimages in
`blobs`. The one-time sweep installed by `rewind::install_checkpoint_sweep` runs when a
chat store is first opened in a process. `AgentTools::new_recursive` installs it, so
every agent host does. It works row by row over the chat's legacy rows, in one
transaction:

- A row whose boundary sequence is at or above its session's keep bound (the previous
  and current turns) is copied into `checkpoint_records`, and its preimage from `blobs`
  into `checkpoint_blobs`. The row is then deleted from `attachments`.
- A row below its bound is deleted.
- A created-file row (no pre-hash) always migrates, even if its empty preimage blob is
  missing. It restores by deletion. The missing blob is replaced by an empty preimage
  written with the normal checkpoint blob encoding, so a later checkpoint of an existing
  empty file reuses it and restores.
- A row whose metadata is not a JSON object or has no boundary sequence has no known
  turn. It is left in `attachments` untouched, and the runner logs a warning naming the
  chat and the row ID. Its session then gets one unrestorable record at the start of its
  latest turn, so rewinding that turn refuses. The record is pruned once two newer turns
  exist.
- A row whose needed preimage blob is missing is migrated as an `unrestorable` record
  when its boundary is in a keep range. It has a NULL `preimage_hash` and no blob, and
  its metadata carries the reason. A row outside the keep range is left in `attachments`
  with a warning.
- Rows left in `attachments` still count toward the high-water mark when their patch
  index is readable, and they export as attachments.
- Rewind targeting a turn that contains an unrestorable record refuses before any fork
  is created. The error names the path ("rewind refused: <path> has an unrestorable
  checkpoint ..."), so a rewind never reports partial success.
- Only SQLite errors fail the sweep. They roll back the whole transaction and fail the
  open.

Then the sweep raises each session's high-water mark as above and sets the chat metadata
key `rewind_checkpoint_prune` to `v2`. The marker is set on the first open even with no
legacy rows, and even when some rows were left in place, so the sweep runs once per chat.
The sweep never changes `blobs`: legacy preimages stay there until the chat is deleted,
and the copied records reference their `checkpoint_blobs` copies. It only opens the chat
it was given, so archived chats stay archived and are swept when they are next opened.

Chats already open when the sweep is installed, including the active chats the directory
store opens at startup, are swept at install. Each cached chat is swept even if another
fails. A failure is logged as a warning naming the chat. That chat stays unmigrated until
a new process opens it.

Refuse without the marker: for a directory-backed chat, rewind checks
`rewind_checkpoint_prune` before any workspace or fork work. If it is not `v2`, rewind
refuses with "rewind refused: this chat's checkpoint upgrade has not run" and forks
nothing. An in-memory store has no legacy layout and never runs the sweep, so it is
treated as already migrated and skips this check. Restore reads preimages only from
`checkpoint_records` and `checkpoint_blobs`.

Export: `export_jsonl` and `export_data_dir_jsonl` write a session's checkpoints on its
first message record, next to its attachments. Each entry carries `id`, `boundary_seq`,
`metadata`, `preimage_hash`, and the preimage as base64 in `data` (`encoding: "base64"`).
An unrestorable record has a null `preimage_hash` and no `data`.
Known gap: a session with checkpoints but no messages exports none, as with attachments.

## User and host commands

- stdio: `session.rewind {session_id, message_id}`.
- Runner library: `rewind::rewind_session(&store, workspace, session_id, message_id)`.
- HTTP: `POST /api/sessions/{id}/rewind` with `{ "message_id": "..." }`.
- User messages: `/rewind <message-id>` through CLI `run --session`, stdio
  `run.start`, `run.steer`, `run.follow_up`, or web message routes.

These are host commands, without a model call, queued user message, approval
prompt or model-facing rewind tool. Empty and unknown targets fail explicitly.
The target is a canonical message ID in the source session and must be the
latest turn start of that session: a typed user row (a run, steer or followup row,
`is_typed_user`) or a `ChildTask` instruction, which agent child sessions receive.
Host notices (runtime, budget, schema correction) are user-role rows too, but they
never count. Any other target, including an older user message or an assistant
message, is rejected before anything is restored or forked, with an error saying
only the last message can be rewound. A child session therefore rewinds its own
latest instruction.
Checkpoint message IDs must still resolve to their recorded sequence positions.
A moved workspace or changed history mapping fails explicitly before the fork;
re-imported checkpoints are not remapped by this feature.

Rewind uses `Store::fork_at` machinery to copy history through the target message,
inclusive, with fresh message IDs and the usual parent link. The original
session's messages and checkpoint records remain unchanged. Existing fork
restrictions apply: active sessions, active background effects, compaction and
cutoffs containing unresolved tool calls are rejected. The target must be the
latest typed user message (see above). A generic `fork_at_with` callback retains the
fork admission guard while the runner restores files. Rewind checks the target and
builds its restore plan under that same guard before the fork is created
(`fork_at_admitted`), so no turn can start between the check and the fork.
The fork carries the todo list as of the selected message, using durable todo
snapshots rather than the source's latest list. A change made inside a tool cell
is included after its completed tool result, not at the owning assistant message.
Copied snapshot boundaries follow the fork's fresh message positions, so later
rewinds of that fork retain the same behavior. A pending question is never copied;
its waiter and answer routing belong to the original session. The original todo
and question state remain unchanged. Todo values written before snapshot support
have no historical record and therefore produce an empty fork list.
That existing guard temporarily excludes runtime admission across the store;
other sessions' starts and deletes can wait for a restore to finish. Existing
external writers remain subject to file hash and compare-and-swap checks.

## Restore behavior

For each path changed by this session's checkpointed changes after the target, rewind walks
the checkpoint chain back to its earliest preimage after that message. The file's
current bytes must equal the last change's post-image; absence compares as absence.
Adjacent checkpoints must also agree, so an outside edit between checkpointed changes cannot
be silently undone. A mismatch, missing preimage, oversized barrier, symlink or
invalid workspace path is a conflict. There is no force option.

Restoration uses the file tools' containment, staging and final compare-and-swap
checks under the shared workspace mutation lock. Existing files are restored,
added files are deleted, and deleted files are recreated. Each path commits
individually; a later conflict does not undo an earlier successful restore.
Filesystem and SQLite commits are separate, as they are for `edit` and `write`.

The result contains the new `session_id`, arrays `restored`, `deleted`, `recreated`,
and `conflicts` (`{path, reason}`), plus a `limitation` string. The source session's
workspace is restored; clients continue the conversation using the new ID.
Rewinding an isolated child resolves that child's existing workspace binding.
Other sessions' checkpoint records and children's worktrees are not restored.
Relative paths in new checkpoints and rewind results use `/` between directory
components on every platform. Older Windows checkpoints using `\` are read as
the same paths, including when a later mutation uses the new spelling. Unix literal
backslashes remain filename characters. These spellings retain the ordinary
workspace containment and symlink checks.

Only changes checkpointed by `edit`, `write` or the former patch tool are
covered. Exec, MCP, user edits and other chats' changes are not rewound. A changed file is preserved as a conflict
when its content no longer matches this session's expected post-image. The result
states this limitation. These content checks are cooperative write protection,
with the same filesystem race limits as ordinary file mutations.
