# Storage model

Status: built through 2026-10-20 (durable request usage, account quota and restart wait markers; phases A, B and C, worker visibility, guarded deletion, session names, temporary runs and archived retention). Phase C adds transcript import,
cold storage, contentless search and buffered serving writes. The native-tool
commit target is met; its phase-A-or-better latency target remains unmet.

## Goal

- **No redundancy:** store each piece of content exactly once.
- **Separation by lifetime:** intensive and disposable data lives apart from the record that must survive, so it can be cleaned by dropping a table or file instead of rewriting the core.
- **Cheap turns:** a turn appends; it does not reload, re-parse or re-serialize the whole history.

## Names, temporary runs and archive retention (2026-10-15)

Session titles use the existing authoritative session JSON and catalog title
projection. Naming and renaming require no schema change; search-hit titles
are read from current metadata rather than copied into the contentless index.
The runner's `--no-session` creates a private temporary store before
opening the configured store. All usual session resources, including spill
files and checkpoint blobs, follow that temporary data directory; normal exit
or an error removes it. A forced process kill may leave temporary OS files.

`[storage] archived_retention_days` is optional and defaults to keeping archived
chats forever. With a value, host startup deletes only root chats whose actual
archive transition is strictly older than that many days; `0` expires archives
at the next startup. This runs after spill cleanup hooks are registered and
before transports or model services start, under the store's exclusive process
ownership. Active chats, idle-but-active chats, recent archives and old archives
whose archive time is unknown remain intact. New roots save this setting in their
root storage policy; legacy roots use the host startup setting. Changing config
applies to newly created roots. Expiry still runs at host startup. The separate
`archive_after_days` idle-compression setting remains.

Each new archive stores `archived_at` in existing authoritative chat metadata
before packing, adding one canonical metadata transaction. The archive-state
catalog transaction also projects this timestamp under `archived_at:<root>`.
Explicit catalog rebuild recovers it from the packed record. Re-archiving after
a restore records a new time. No schema changes or message rewrites are needed.
Deletion uses the existing whole-chat deletion and host cleanup callbacks, so
descendants, blobs, checkpoint attachments and spill folders are removed too.
Each deletion is logged to stderr. Filesystem cleanup failures are reported
after canonical deletion, following ordinary deletion semantics.

Session-list summaries read archived records through the existing read-only
temporary-unpack path, sharing one unpack per root within a bounded list page.
Counts and latest-message/run metadata remain accurate for roots and children
without restoring their chat files. The web sidebar checks pending questions
only in active sessions, so merely listing chats preserves their archive age.

`tests/session_lifecycle.rs` covers the real CLI, stdio and local HTTP naming
paths, byte-for-byte isolation of the real store, temporary spill cleanup,
default retention, eligibility boundaries, rebuild and re-archive, and complete
expired-chat resource deletion. It also reports archive bytes, WAL and
transaction counts. This is scripted acceptance without live model calls.

## Bounded host state (2026-10-15)

An additive `host_state(session_id,key,value)` table keeps small runner-owned
state in the authoritative chat file. Values are bounded to 64 KiB and replaced
atomically with their `host_state_changed` event under the snapshot lock. The
runner uses one row for the latest todo list and one row for question/answer
state; the engine does not interpret either. Todo calls remain ordinary history,
without repeated prompt injection. Directory-store event snapshots live in the
disposable serving journal. Question state retains only its latest value.
Todo additionally opts into immutable state snapshots in the existing attachment
and content-addressed blob tables so rewind can select its value at a canonical
message boundary. Each snapshot, latest row and event commit in one transaction;
equal snapshot values share a blob within the chat. Forked snapshot boundaries
are remapped to the fork's history, allowing another rewind there. State follows
chat-file archival/restoration and is deleted with its session. Existing schema
is neither changed nor removed.

Question recovery admits accepted generated answer input, deduplicated by a
question ID marker. Unknown external effects are retained and never replayed.
Scripted recovery checks are listed in [verification](verification.md).

## Transient worker histories (2026-10-14)

Internal workers are ordinary hidden child sessions in their root chat file.
The generic hidden flag uses existing Session JSON; no SQL schema migration is
needed. The runner owns worker metadata, delivery acknowledgement and retention.
Success is deleted only after caller acknowledgement. Failures remain inspectable
for seven days by default, with unresolved effects preventing automatic deletion.

Worker deletion uses the existing subtree delete, with a generic unresolved-effect
guard inside the canonical transaction. It invalidates discarded session caches,
reclaims free chat pages through incremental vacuum and checkpoints the chat WAL.
Catalog/serving housekeeping remains disposable. Its acceptance checks, which assert
transaction counts and WAL behaviour, are in [verification](verification.md).

## Scratchpads (2026-10-28)

Scratchpads are disposable files under the host data directory, outside SQLite:

- `scratch/chat/<root-chat-id>/` is shared by a root chat and all descendants.
  Deleting a child preserves it; deleting the root removes it through the
  existing session-cleanup callback. Archives preserve it. Cleanup is registered
  on host startup, including delete-only CLI invocations after restart.
- `scratch/workspace/<hash>/` is shared by chats using the same canonical
  workspace root. The key is SHA-256 of that canonical path (native Windows
  UTF-16 units or Unix path bytes). A child using another worktree gets that
  worktree's project scratchpad while keeping its root chat scratchpad.

The project sweep runs alongside process-spool sweeping at host startup. Its
constant retention age is **seven days since last use**. It removes a project
directory only when the directory and every nested entry have been unmodified
for at least seven days and no live host holds its workspace lease. Acquiring
or using a workspace refreshes `.last-used`; the host acquires its current
workspace lease before sweeping. Shared cross-process file locks protect live
hosts; the sweeper takes an exclusive lock before checking age and deleting.
Lease files and the sweep lock live outside removable project directories.
The sweep does not follow symlinks or junctions. No config knob is added.

The runner owns paths, file grants and cleanup. `WorkspaceTools::scratchpads`
resolves the paths for a session; `prepare_scratchpads` binds a host data dir and
retains a workspace lease. The native pack does this at startup. Low-level file
tools use the dispatcher's data dir when none is configured; in-memory fixtures
without either source use an owned temporary directory. Embedders constructing
their own prompts can use `root_prompt::configure_scratchpads` with the same
workspace tools to show the grants.

Scratch file operations add no workspace rewind checkpoints or scratch-content
SQLite records. Their accepted tool calls, completed results and ordinary effect
boundaries remain durable. An interrupted exec with an unknown outcome is never
replayed. See [file tools](file-tools.md) and [exec](exec.md).

## Large tool output (2026-10-15)

Tool output over the existing 64 KiB dispatcher cap is retained as UTF-8 text in
`spill/<chat-id>/<id>.txt` under the BubblePaw data directory. JavaScript return
previews use the same store, including token-budget spills smaller than 64 KiB.
The durable tool message contains a bounded marked head/tail preview, an absolute
path and total saved bytes. The full output does not become a SQLite blob, an
attachment or a publication, and saving or evicting a spill performs no SQLite
write transactions.

The folder key is the owning session ID. Each session, including each child
session, has its own 256 MiB filesystem retention cap. Saving another output evicts
oldest files first; an output larger than the cap fails explicitly. Old references
may therefore become unavailable. Deleting a chat removes its spill folder and
folders for descendants deleted by the store; archiving preserves them. Runner
hosts register cleanup when opening their store, including CLI-only deletion
after a restart. Core exposes only a generic post-deletion cleanup callback and
a generic pre-cap result limiter; it has no spill schema or directory policy.
Canonical deletion and filesystem cleanup are separate operations. A cleanup I/O
failure is reported after canonical deletion has completed.

Large exec output is preserved from the complete bounded job spool before it is
reaped. Preview gaps are separate from spool loss. Capture beyond the configured
job spool cap cannot be reconstructed and produces `fullOutputError` rather than
a false complete-output reference. See [exec](exec.md) and [CodeMode](codemode.md).
The scripted `tests/spill.rs` fixture measures retained text bytes, database/WAL
bytes and application transaction counts; dated results belong in verification.

## File checkpoints (2026-10-15)

`edit` and `write` store per-file immutable checkpoint metadata and original bytes
(the same records previously used by the former patch tool)
through the same attachment/blob APIs as images. Blobs deduplicate by content
within the chat file; checkpoint rows link the owning session, invocation,
canonical message boundary, workspace and pre/post hashes. One changed file adds
one attachment transaction before target writes. Oversized files receive a
durable barrier without original content. Existing session deletion prunes
attachments and unreferenced blobs; archives retain them. There is no SQL schema
change. See [rewind](rewind.md) for the 10 MiB cap, safety checks and scope, and
[verification](verification.md) for the checks that run.

The warmed Linux fixture for direct native dispatch changes a synthetic,
repetitive file of exactly 20 KiB
50 times. Its 50 distinct preimages total 1,024,000 raw bytes and 1,250 compressed
bytes. The checkpointed chat grows from 262,144 to 512,000 bytes; all store files
grow from 405,504 to 884,736 bytes. The turns use 300 application transactions
(250 chat, 50 serving, zero catalog), including 50 checkpoint writes. Sampled
within-turn WAL peaks at 2,204,296 bytes; after the turns it is 1,421,496 bytes
and an explicit idle checkpoint returns it to zero. These are fixture costs,
not a compression estimate for arbitrary source files or live-model acceptance.

## Image attachments (2026-10-15)

Image reads use the existing `attachments` and `blobs` tables and the existing
`Store::store_attachment` / `Store::attachment` API. No SQL schema changes are
needed. Each original has a SHA-256-derived `image-<hash>` attachment ID. Metadata
contains its MIME type, byte count and originating session; it excludes the
mutable workspace path. Identical bytes read through different paths share the
same attachment row within a session and the same compressed blob in the chat
store. Reading an already-stored image adds no attachment write transaction.

Canonical tool messages retain only small references in a line-delimited host
envelope. Their content, durable events, exports and observation trace never
receive image base64. Original bytes are encoded only in a transient provider
request, after core observation and persistence. Each prepared request resolves
its references from SQLite, so reopening a store and deleting the original file
does not lose the image or rerun the read. Repeated references in a request send
the actual image once.

References carry their origin session because a fork rewrites message session
IDs without copying attachments. Fork replay can therefore read the original
session's retained attachment. Forks do not gain independent image retention;
preserving images after their originating session is deleted is outside this
change. A missing attachment in a retained origin produces an explicit note.
Store retention semantics remain unchanged.

`tests/images.rs` measures original bytes, compressed blob bytes, database/WAL
bytes and transaction count for repeated reads. It checks one original blob,
zero attachment transactions on repeat storage, reference-only messages, actual
close/reopen replay and fork provenance. Provider-specific wire shapes and
explicitly gated live acceptance are in `tests/provider_images.rs`; see
[image input](providers.md#image-input-2026-10-15) for the limits and capability
override. Reported verification results belong in the dated verification page.

Linux fixture measurement on 2026-10-15: a 256,000-byte original occupies one
256,012-byte compressed blob. After warming the ordinary store, its database
files grew from 405,504 to 692,224 bytes (286,720 bytes) across attachment storage
and eight read turns. The checkpointed WAL was zero bytes. The fixture used 58
total transactions; twenty repeated direct attachment stores added zero
transactions. These are fixture measurements, separate from live model acceptance.

## Web retrieval storage (2026-10-13)

Web source IDs and URL/title mappings are immutable attachments in the root
chat record, shared by its children through existing Store APIs. They survive
reopen and cache deletion without adding a core schema. Successful search replies
and converted page text with numbered passages live separately in disposable
`web-cache.db` beside `catalog.db`. It never stores raw HTML or authoritative
source mappings. TTL and LRU eviction bound reuse and retained response data;
see [web tools](web.md) for configuration and measured file/WAL sizes.

## Before phase A (survey 2026-10-12)

- **Duplicated tool results:** stored in `messages` and again in `events` (ToolFinished, up to 64 KiB each).
- **Duplicated final text:** stored in the reply message and again in RunFinished.
- **Duplicated arguments:** tool-call arguments appear in messages, events and approvals.
- **`provider_state`:** repeats the reply's text and tool calls next to the signatures it exists for.
- **`context_views.prefix`:** a full copy of visible history, rewritten per compaction.
- **Events:** never pruned. Deleting a session never reclaims space (no VACUUM).
- **Per model round:** the whole session is loaded and parsed, and the whole request is re-tokenized and re-serialized.
- **Per turn:** about 7 fsyncs.
- **Size:** about 3 KB per message on disk; Needle, 2,492 messages, is 8 MB.

## Central store (2026-10-21)

Each user and machine has one central store, by default
`<local data home>/BubblePaw-Core/store` (`LOCALAPPDATA`, then `XDG_DATA_HOME`,
then `HOME/.local/share`, or `HOME/Library/Application Support` on macOS). It holds every chat from every project; each root
chat carries its own `root_workspace:<root-id>` binding. The first process to
take its lease hosts it, and later `run` and `delegate` processes
[attach](host-attach.md). Shared state (`quota_history` in `catalog.db`,
`delegations.db`, `web-cache.db`) lives in this store and is written by the host.
`--data-dir` or `storage.data_dir` selects an isolated store.

### Windows: packaged parents (2026-10-21)

**MSIX file redirection.** A process started from a packaged desktop app runs
in that package's context. The Claude desktop app is one, and so are its Git
Bash and PowerShell children. Its manifest leaves file-system write
virtualization on for `LocalAppData`, so the central store path depends on who
created `%LOCALAPPDATA%\BubblePaw-Core` (then named `BubblePaw-base`). Measured on Windows 11 26200 with
Claude `2.42304.2.0`:

- If the folder does not exist in the real `AppData\Local`, a packaged process
  creates it in `AppData\Local\Packages\<family>\LocalCache\Local`. Packaged
  processes see a merged view. Unpackaged processes see only the real
  location, so they get a second, empty store.
- If the folder exists in the real location and has no private copy, packaged
  processes write new files and subfolders inside it to the real location.
  Then there is one store for everyone.
- If a private copy exists, it wins for packaged writes, even after the real
  folder is created.
- The redirection follows the target path. A junction elsewhere pointing into
  `AppData\Local`, a `\\?\Volume{..}` path, or creating the folder elsewhere and
  renaming it in all stay redirected. `PROC_THREAD_ATTRIBUTE_DESKTOP_APP_POLICY`
  with breakaway did not escape either. Only processes started by a broker
  outside the package escaped: WMI `Win32_Process.Create`, and presumably Task
  Scheduler.

So there is one store per machine when the real
`%LOCALAPPDATA%\BubblePaw-Core` folder exists before any packaged process
creates it, and no private copy exists. This was checked with the fixed exe.
A packaged process wrote a central store whose parent folder an unpackaged
process had created first. An unpackaged process (WMI) then listed the same
sessions. Without the real folder, the packaged process wrote into `LocalCache`,
and the unpackaged process got an empty store. A redirected store is visible
from inside: the canonical data directory resolves under
`AppData\Local\Packages\<family>\LocalCache\`. BubblePaw does not ensure this yet; how it should is an open decision.

## Durable account quota and wait markers (2026-10-20)

`catalog.db` now also contains authoritative `quota_history`, independent of any
chat. Its session/catalog projections remain rebuildable; its quota history must
be preserved during rebuild and must not be treated as disposable. Readings are
keyed by account/window, with provider attribution retained on each change. A
row is written only when percentage or reset changes; repeated readings and
provider/source-only changes add no transaction or WAL growth. Live observation
ages stay in host memory. No quota data is stored on messages.

A quota wait keeps a small marker in the owning chat's metadata, alongside its
authoritative `waiting_for_quota` run record. Marker/status/event share one
canonical transaction. Clearing before retry uses one marker transaction;
ordinary finish/failure removes any marker in its existing final transaction.
Explicit cancellation clears a pending marker synchronously in one transaction
before acknowledging cancellation. Empty clears never start a transaction.
Wait records cannot be moved into
disposable serving storage or archived while pending.

After an interrupted quota wait, the host schedules the existing resume path,
which creates a new run. Atomic expected-marker admission prevents cancelled or
replaced waits from being resumed. Unknown effects prevent automatic recovery.
This is a small recovery hint, not a serialized request or continuation state;
run counters restart after process loss. [Quota](quota.md) records the policy,
measured costs and verification boundaries.

## Durable request usage (2026-10-20)

Schema 8 adds `request_usage`; schema 9 adds account identity, backfilling earlier
rows from their provider name. A row in each authoritative chat file retains a
stable UUID, UTC request time, original session ID, optional run ID, actual
provider/account/model, existing request kind, input/cache/output/reasoning counts and a
failure flag. It stores no prompt, response body or price. Input includes cache
reads and creation; reasoning is already part of output. Every physical
continuation, retry and fallback is a separate row. Custom providers without
physical observations retain one row per provider invocation.

Normal main responses commit their rows in the same FULL transaction as the
assistant message, tool dispatch boundary or terminal step. Run totals and
`RunFinished` are unchanged. Auxiliary calls stage rows immediately before their
existing summary/checkpoint/decision write. A standalone transaction is needed
when an auxiliary call has no canonical write, fails without producing a derived
record, or must finish one operation before starting another. Dropping a capture
preserves completed attempts through a small fallback transaction. An interrupted
provider cannot report tokens it has not returned.

The ordinary final-turn contract is two chat commits plus one serving commit;
the existing single-Allow-tool turn remains four chat plus one serving, zero
catalog. Usage adds zero normal-step transactions. Automatic-approval usage shares
the existing approval insert. The focused fixtures assert these counts, atomic
rollback and serving-wipe survival; current execution evidence
is recorded in [usage tracking](usage-tracking.md).

Rows survive summary replacement, run/event pruning, serving wipes, child-session
cleanup, compression and archival. Session IDs intentionally have no foreign key:
automatic deletion of a hidden worker must not erase billed requests. Forks do
not copy usage. Deleting the root chat file removes its usage with the rest of
the chat. Read-only reports include packed archives without restoring them.

The obsolete, never-written serving `usage` table is dropped on opening the
serving layout. Existing diagnostic aggregate copies remain disposable and are
not the billing source. [Usage tracking](usage-tracking.md) documents read-time
pricing, filters and the fixed JSONL export.

## Tiers

### Compaction facts and message tokens (2026-10-27)

Chat schema 12 adds `context_events(id TEXT PRIMARY KEY, session_id TEXT NOT
NULL, kind TEXT NOT NULL, data TEXT NOT NULL)`, with a session index and a kind
check for `episode_written`, `chapter_closed` and `applied`. `data` is serialized
`ContextEvent`; reads order by insertion `rowid` so timestamp ties are stable.
Its `tokens_before` is null when inherited coverage lacks stored source counts;
cold originals are never loaded just to obtain this metric.
A new chat starts with an empty event table. Events are durable chat data, including when `context_views` lives
in the disposable central `serving.db`.

The chat schema also has a nullable `request_usage.message_seq` and a partial
index on `(session_id, message_seq)`. Successful assistant-producing requests
link to their canonical message in the existing message transaction. A response
joined from several successful continuation requests links each contributing
row, and the read sums `output_tokens`. Usage without a message stays unlinked;
missing usage stays absent in core's reported-count read. The SDK estimates it
on read using the runner's estimator, without persisting a zero measurement.
Explicit provider-reported zero remains a reported count. Discarded fallback
output retains its billing row but is excluded from message attribution.

`ContextView` JSON adds defaulted `blocks` and `blocks_estimated`; no block table
is added. Old rows without blocks continue to load. The runner records those
blocks when it builds an applied view, with IDs and source ranges for summaries
and original-message blocks.

Gather events share `save_episode_batch`'s transaction. Apply events share the
chat transaction already used to validate/encode a view before saving its
serving projection. Message links share existing assistant writes. These facts
therefore add zero gather transactions and zero ordinary-turn transactions;
view reuse emits no event and no write. See [context compaction](context-compaction.md)
for trigger rules and token meanings.

`context_compaction` journal notifications reference the committed fact by ID,
kind and trigger. They share its canonical write boundary; central stores buffer
them into the serving transaction already required by catalog projection work.
Multiple facts share one flush, with no extra chat or standalone serving commit.
The existing disposable-journal gap and resnapshot behavior remains unchanged.

| Tier | Holds | Lifetime | Cleanup |
|---|---|---|---|
| **1. Record** (must) | Sessions, messages (append-only), content blobs, accepted inputs, effect boundaries, crash-safe pending state and per-request usage | Forever, until the user deletes the session | Session delete plus vacuum |
| **2. Derived** | Summaries, chapters, episodes, decisions, as seq ranges plus text | Kept: costs money to recreate | Per strategy or per session; never needed to read history |
| **3. Operational** | Event journal, completed runs and resolved approvals; pending state belongs to the record | Short retention | Pruned automatically |
| **4. Telemetry** | Timings, diagnostic aggregate usage copies and reasoning traces of internal calls | Optional | Drop any time |
| **5. Serving view** | What the model currently sees | Disposable | Wipe serving and rebuild from tiers 1 and 2 |

### Rules

- **Canonical content lives in tier 1 only.** Every other tier references it by `(session, seq)` or a blob hash and never copies original text, arguments or outputs. Derived summaries retain their own text.
- **Event journal:** holds kinds, ids and refs, for example "tool finished → message 812". Resume uses chat records and does not require the journal.
- **Messages:** keep what the provider must receive back byte-exact (thinking blocks, signatures, tool ids) inside the message, once. `provider_state` keeps only what is not already in the message.
- **Large payloads:** tool outputs and attachments above a threshold go to a blob table keyed by content hash. Identical outputs are stored once, and the hot message table stays small to scan.
- **Effect boundaries:** an effect row records status and call id; the outcome is the tool-result message. After resolution, only the minimum needed to refuse replay stays.
- **Physical separation:** disposable tiers 3–5 live in `serving.db`. Accepted inputs, pending approvals and other crash-safe state live in the chat file until safe to discard or archive. A chat file alone must be enough to read, resume and export its sessions.
- **Turns append:** the runtime keeps parsed history and per-message token counts in memory, and adds new messages. One durable step's record writes share one transaction; serving journal writes follow that commit.
- **Housekeeping:** retention pruning, idle checkpoint, and vacuum after deletes.

### Reasoning artifacts

Rule (2026-10-13, overrides the earlier import drop): every provider reasoning
artifact is kept byte-exact on every path, native and imported, and never
dropped. That covers Responses `reasoning` items (`id`, `encrypted_content`,
summaries), Messages `thinking` with `signature` and `redacted_thinking` with
`data`, chat `reasoning_content` from every round, and any other opaque field a
provider returns. Proprietary models need them back to continue a conversation.

The rule covers every artifact of a completed message, including a thinking
block cut unsigned at max_tokens (stored, marked `unsigned_cut`, never sent).
A stream that is cancelled or drops before its response completes stores no
assistant message, so its reasoning is not kept: stream fragments are transient
(AGENTS.md) and an interrupted turn is never replayed (decision 2026-10-14).

- **Where:** opaque provider state on the assistant message it belongs to, stored
  once. Text and tool calls inside it are references to the canonical message.
  A `provider_state` over 4 KiB moves to the blob table (`_provider_state_blob`)
  like other large content.
- **Origin tag:** `provider_state._bubblepaw_origin = {provider_kind, model,
  source}` with kind `responses`, `anthropic-messages` or `chat-completions`,
  the exact request model when known, and source `native`, `import:codex` or
  `import:claude-code`. The tag sits beside the artifacts, never inside them.
  States written before the tag are treated as native to their adapter.
  The host's `_bubblepaw_route_origin` follows the same compatibility rule: an
  absent route tag inherits the current route when adapter/model match; only a
  recorded route/endpoint mismatch is incompatible.
- **Paths:** store, dictionary-row (cold) compression, archive, unarchive,
  reopen, export and canonical re-import all return
  identical signature and ciphertext bytes, and the next Responses or Messages
  request body equals the one built before storage
  (`crates/bubblepaw-runner/tests/reasoning_storage.rs`).
- **Replay is not storage.** Adapters send an artifact only to a compatible
  provider and model ([providers](providers.md#reasoning-artifacts-2026-10-13));
  the store keeps everything regardless. Current Messages tool-loop turns need
  their compatible provider state; missing or foreign state produces a local
  error instead of silently reconstructing a turn without signed thinking.
  Chat defaults to replaying current-loop reasoning. Neither policy changes
  storage or its schema.
- **Compaction:** reasoning from turns that a summary replaced stays in the store
  but is not replayed, as the summary replaces those turns. Rewind, branch and
  LCM expansion read the stored originals.

## Layout (decided 2026-10-12)

```
bubblepaw/
  catalog.db        session catalog plus authoritative account quota history
  serving.db        disposable: live views, token-count caches, telemetry, request log, event journal, search index
  web-cache.db      disposable: shared web search replies, readable page text and passage splits
  chats/
    <chat-id>.db    one top-level chat plus every sub-agent it spawned: messages, blobs, summaries, effect markers
```

- **One central store (2026-10-21).** By default every workspace uses
  `<local data home>/BubblePaw-Core/store`
  ([central store](#central-store-2026-10-21)).
- **One host process.** A frontend is a host that runs many sessions in one process. Sub-agents are sessions in the same process, not separate instances.
  Later `run` and `delegate` processes [attach to that host](host-attach.md)
  through a user-only local endpoint. Election still uses the unchanged core
  process lock; the host owns shared runtime shutdown and waits for its clients.
- **Chat files (`chats/<chat-id>.db`).** A top-level chat and its whole delegation tree share one file. Parent links are ordinary rows, deleting a chat deletes its tree, and one large chat never bloats another. SQLite's single writer is fine here: writes take milliseconds, model calls take seconds.
- **Per-chat workspace ownership (2026-10-21).** A root binds its canonical workspace
  at creation in `root_workspace:<root-id>` chat metadata, in the session creation
  transaction. Children inherit it unless an existing child worktree provides
  their execution directory. Protection uses that root and its child worktrees,
  never an unrelated session's or the host's startup workspace. Catalog rebuild
  accepts different workspace bindings and rejects malformed ones. Legacy chat
  `workspace` metadata and the catalog-level binding remain readable fallbacks;
  old chat files need no rewrite or manual migration.
- **Per-chat storage policy.** New runner roots store `root_storage:<root-id>`
  beside the workspace binding. Cold-storage thresholds, idle archival and
  retention use that root's policy; catalog metadata is a rebuildable projection.
  Housekeeping still uses one serving transaction for all eligible roots.
  Chats without a saved policy retain the old host-default behavior. Whole-archive
  expiry uses the saved `archived_retention_days` at host startup.
- **No consolidation.** A child's result already reaches its parent as a tool result.
  - To read a sub-agent's exact chat: session id → `catalog.db` → chat file → that session's messages.
  - `bubblepaw export <session>` writes it as JSONL for reading and other tools.
- **`catalog.db`** has small session projections rebuildable from chat files and authoritative quota history. Rebuild preserves quota history; deleting this file would lose it.
- **`serving.db`** is cleaned continuously. The user can wipe it at any time; its tables are rebuildable from chat files or acceptable to lose. It holds tiers 3–5. Only what is needed to serve stays there.
- **Derived summaries** live in the chat file, next to the messages they cover. LCM summaries are small and cost money to recreate.
- **Blobs:** tool outputs and attachments over 4 KiB go to a content-hash blob table in the chat file, zstd-compressed.
- **One schema per store.** Every store file is created at its final schema and opens only at exactly that version (see below); there is no upgrade path from other layouts.
- **JSONL** is an export format only; SQLite stays authoritative (AGENTS.md).

### Ownership release

One process owns a data directory through an exclusive `store.lock` lease.
Repeated opens in that process share the live Store; clones retain ownership.
Dropping the final Store synchronously flushes buffered projections, closes
store state and SQLite connections, explicitly unlocks the lease, and closes
its descriptor before returning. Explicit unlock also releases a Unix flock
when an unrelated fork temporarily inherits its open file description before
exec; close alone does not provide that guarantee. Migration source and
destination leases and idle export's shared reader lease use the same
explicit-unlock guard. An export under a live host still uses SQLite's WAL
snapshot without acquiring a shared lease.

A failed weak-reference upgrade proves there are no remaining Store users,
but does not prove their destructor has finished. The process registry retains
a teardown completion signal. An opener waits on that signal before acquiring
the file lock; it never retries a live owner's OS lock or relies on a sleep/PID
heuristic. The lease is the final field destroyed, so its signal follows store
cleanup and descriptor closure. Another process still receives an immediate
ownership error while a live owner holds the lease.

Hosts stop and join their work before dropping their Store handles. Harness
shutdown or executor destruction alone does not close Store clones held by a
REPL thread. Interrupted-executor recovery tests join that thread on a fresh
executor after destroying the old one, preserving unknown effects without
running their old completion observers. An interrupted external effect is
never replayed.

### Cold-storage seams (2026-10-12)

Phase B established these boundaries. Phase C implements compression behind
them; callers continue to resolve chats and decode messages through the same paths.

- Chat-file access has one opening boundary. Normal access resolves session id
  through the catalog to its path and storage state, then opens through that
  boundary. Read-only export uses the same boundary. New-file creation and catalog
  rebuild use its explicit bootstrap path because their catalog row does not yet
  exist; they do not introduce another SQLite opening implementation.
- Catalog sessions carry `storage_state`: `active`, `archiving` or `archived`.
  The state applies to the root chat file and its child sessions.
- Stored message bodies pass through one decoder, including history, snapshots,
  indexed tails, event hydration and export. The dictionary row codec lives
  there, without changes to those callers. Existing
  large-content blobs retain phase A's separate zstd representation.

## Phase B implementation

The library opens a directory with `Store::open_data_dir` or
`Store::open_with_options`. `create_session_with_parent` accepts an optional
parent session id; it changes placement only. The runner owns child execution.
`Store::open(file)` and `--db` select a sibling `<stem>.data` directory; neither imports anything.

Chat files use schema 12 (message content formats, `request_usage`, a nullable
checkpoint preimage, a tool-call index that separates calls from results, and
compaction facts), with a `chat-v1` layout marker and
`auto_vacuum=INCREMENTAL`. They hold messages, attachments and compressed blobs,
summary records and frozen checkpoint versions, durable per-request usage, effect markers, pending inputs,
approval rules, and operational state still needed for recovery. Accepted input
content transfers to its canonical message when consumed; the consumed queue
metadata moves to serving. Interrupted runs remain in the record. Completed
effects retain the boundary needed to refuse replay, rather than repeated detail.
Workspace bindings live in chat metadata and survive rebuilding both projections.

Each store file has one schema: chat records at version 12, `catalog.db` and
`serving.db` at 2, and the delegation ledger at 1. A missing file is created
directly at its final version: the DDL, the application id and `PRAGMA
user_version` are written in one transaction. An existing file opens only if its
version is exactly the one the binary writes. Any other version, older or newer,
fails with `StoreSchemaError { store, found, expected }` (re-exported from
`bubblepaw_core`; the ledger's error is the same type), wrapped in `anyhow` so a
caller reaches it with `err.downcast_ref::<StoreSchemaError>()`. The check runs
before any pragma touches the file, so a refused open changes nothing. A file
that is not a BubblePaw store keeps its separate foreign-database error. Tables
that are created per open without a version bump (approvals, the event log, host
state, reminder metadata, dictionaries; the request log in `serving.db`) are
`CREATE ... IF NOT EXISTS` statements in the open path, not part of the
version.

To change a schema later, put the new shape in the file's creation DDL, raise its
version constant, and add a numbered step that upgrades the previous version in a
single transaction with its version bump. Until then, a store written by an
earlier layout has no path forward: create a new data directory.

`serving.db` owns journal references, pointer views, resolved input/approval
metadata, completed runs, and summary usage/internal reasoning telemetry. Reads
hydrate references from the owning chat. Wiping serving loses operational history,
diagnostic usage copies and the old frozen view, while durable per-request usage, canonical history and paid summary content
remain. The ordinary scripted-tool continuation retains identical model-visible
input; compacted contexts rebuild from retained summaries. Arbitrary historical
view choices or a custom frozen prompt are not durable record data.

Record commits precede catalog and serving writes. Phase C batches the two
disposable projections through an attached connection; this does not provide an
atomic cross-WAL durability guarantee. A failed journal write cannot roll back accepted input or trigger
another effect invocation. It invalidates replay coverage and leaves the event
live-only. Session-api clients must resnapshot after a journal wipe or gap; see
[session-api](session-api.md).

The catalog stores session id, parent id, root id, title, creation time, last
activity, relative file and `storage_state`. New root chats create a file;
descendants share it. Startup scans repair missing/stale active rows and leaves
known archived trees packed; missing archives are discovered read-only. Explicit
`bubblepaw catalog rebuild` explicitly rebuilds them. The placement/open gateway
is `open_session_record`; physical record opens go through `open_record`.
`Store::scoped` caches a session's resolved root while its chat is open, so repeat
access skips the catalog query until a rebuild, archive or deletion drops the entry.
`decode_message_body` is the shared body-codec boundary, including metadata
filters and blob reachability; `message_from_data` then hydrates content.

Startup and idle cleanup retain finished-run journal entries for 24 hours after
completion and diagnostic usage/timing telemetry for 30 days by default. Durable
`request_usage` rows have no time-based retention. Resolved input
metadata is kept while retained journal references need it. Idle housekeeping
checkpoints WAL files and vacuums disposable free pages. Root deletion removes
the entire file/tree and its projection rows; child deletion removes its subtree,
collects unreachable blobs and fully steps incremental vacuum. Other chats are
unaffected. Retention settings and offline commands are in [config](config.md).

Export uses the shared placement boundary read-only and streams rows. Message
lines include session/parent/root/direct-child links; the first includes original
attachment blobs as base64. Imported chats also emit tagged source-derived
records and provenance. An ordinary empty history produces no lines; a
compaction-only import still exports its receipt and derived records.
Replay writes its directory layout under `<out>/store`; recall tooling observes
filesystem sizes and runner progress without opening new chat files directly.

### Phase B verification (2026-10-12)

All Rust checks ran on Linux with toolchain 1.97.1: workspace formatting,
strict workspace Clippy across all targets, and `test --workspace` passed
(575 passed, 0 failed, 9 ignored). Strict Clippy with the `web` feature and all
12 web attachment/transport tests passed. The recall Python suite passed all
37 tests with the pinned Python requirements on Linux.

The eleven storage acceptance tests cover physical serving deletion followed by
reopen, identical canonical history and the next prepared request, crash-safe
accepted inputs and approvals, refusal to replay an unknown effect, root/child/
grandchild placement, catalog deletion/rebuild equality, root tree deletion,
chat-file shrink after child deletion and shared-blob preservation, default retention, exact JSONL
message/blob/attachment export and exact-version store opening. Additional unit tests cover
ownership, interrupted root deletion and terminal approval routing after archival.

The ordinary-history request test compares serialized bytes before assigning the
provider-cache cursor; the fresh process's `HistoryCursor.epoch` is ephemeral.
Its dispatched request matches apart from that cache hint. Compacted LCM and
classical contexts rebuild from retained summaries without model calls and retain
the same model-visible message content, roles, tool calls and provider state;
generated projection IDs and timestamps are not durable record identity.

No paid calls, native Windows runtime tests or production-data migration
are claimed.

### Phase B measurements (Linux, 2026-10-12)

The matched Needle prefix has the same source and prefix SHA-256 hashes as phase
A: 2,492 original messages plus one neutral archive-gap marker, 256,000-token
threshold, eleven compactions, 33 synthetic summary calls and 15 synthetic
decisions. Both runs completed without failures; input/output estimates remain
368,548 / 10,307 tokens. Private transcripts, databases and rendered views stay
outside the synced checkout.

| Metric | Phase A | Phase B |
|---|---:|---:|
| Retained chat file bytes | Part of single store | 4,689,920 |
| Retained serving bytes | Part of single store | 114,688 |
| Retained catalog bytes | — | 24,576 |
| Total retained database bytes | 4,706,304 | 4,829,184 |
| Replay wall seconds | 2.4608 | 2.5073 |
| Process wall seconds | 2.5693 | 2.6161 |
| Chat application commits | 110 | 110 |
| Serving / catalog application commits | Included / — | 47 / 111 |
| All-file application commits | 110 | 268 |
| Reported Store commits (excludes catalog) | 110 | 157 |
| Runtime WAL bytes across all files | 1,048,576 | 1,868,520 |
| Sampled WAL peak bytes (5 ms) | 1,194,832 | 1,924,136 |
| Synthetic native-tool turn application commits | 5 | 21 (7 per file) |
| Synthetic native-tool turn wall seconds | 0.005005 | 0.011355 |
| Native-tool turn WAL bytes on return | 0 | 0 |

The split retains 2.6% more bytes overall and this replay sample takes 1.9% more
time. The canonical chat workload still commits 110 times; separate projection
writes raise the all-file count to 268. The native turn adds record cleanup and
separate serving/catalog writes. These are successful application commits,
including validation/no-op and startup housekeeping boundaries, excluding schema
initialization; they are not counts of fsync calls or SQLite's implicit maintenance
transactions. This is one timing sample with scripted models, not a latency
distribution or live-model result. Sampled WAL peaks are lower bounds. Phase B
rendered views were not paired against phase A's private rendered artifacts;
exact reconstruction and model-visible recovery are checked by the Rust tests.

## Phase C implementation

### Transcript import

`bubblepaw import <claude-code|codex> <file> [--title <title>]` creates one root
chat. Adjacent Claude sub-agent files and Codex files with matching native parent
IDs become children in that same chat file; repeat `--child <file>` for explicit
children. Three bounded passes inventory, validate and import JSONL. The maximum
line is 64 MiB; record batches stop at 128 messages or 2 MiB. Native user,
assistant and tool content becomes canonical messages, with imported payloads
over 4 KiB using the existing compressed blob store regardless of role.

Source compactions and attachment metadata are retained as derived records
with `origin="import"`. They are source artifacts,
not BubblePaw-generated summaries. The latest usable source summary seeds the
serving history alongside its canonical tail. Original history remains available.
Injected harness wrappers, copied request history and transient events have
counted exclusions. Reasoning artifacts are kept, not dropped (converter
`bubblepaw-transcript-v2`, 2026-10-13): Codex `reasoning` items and Claude
`thinking`/`redacted_thinking` blocks become assistant records whose origin-tagged
provider state holds the artifact byte-exact, and Codex function-call extras
(ids, `encrypted_function_args`) stay on the call's state. See
[reasoning artifacts](#reasoning-artifacts).
Import metadata records source path, format/converter version, SHA-256 and
normalized message/derived hashes. Streaming export checks both normalized hashes.
See [configuration and commands](config.md#transcript-import).

### Whole-chat archives

Idle housekeeping archives a chat tree after 30 days without activity by
default; zero disables the policy. Explicit `archive` and `unarchive` commands
use the same code. Pending work and other open record handles prevent archival.
After a WAL checkpoint, a private staging file is zstd-compressed, checked against
the source SHA-256 and installed with directory synchronization. Catalog state
advances `active → archiving → archived`; the original is removed only after
verification. Startup recovers interrupted transitions using the surviving
original or verified archive. Known archived trees remain packed during startup.

The normal chat-open path restores through a verified temporary file, atomically
installs SQLite and sets the tree active. Read-only export instead uses a temporary
unpacked copy and leaves the archive and catalog state intact. Explicit catalog
rebuild can read archives without installing their unpacked files. No VFS,
page-level compression or external SQLite extension is involved.

### Dictionary rows and lazy history

A chat qualifies at 1,024 messages **or** 8 MiB of encoded row bodies. Eligible
rows must precede a retained summary/compaction boundary, be outside the frozen
serving view and be older than the newest 128 messages. Rows below 256 bytes,
oversized rows above 64 MiB, and encodings that do not save space remain plain.
These defaults avoid dictionary overhead on small chats; the fixture results
below show additional savings beyond the already compressed large-content blobs.
All thresholds are configurable through `storage.cold_*` keys.

One dictionary per chat file is trained from at most 1 MiB of eligible samples,
with a maximum size of 16 KiB, and stored inside SQLite. Each row carries its
dictionary hash, original length and SHA-256 in a `zstd-dict-v1` envelope. The
shared decoder validates these fields before ordinary blob hydration. Each
open chat and read-only iterator pins its dictionaries until it closes. A weak
process registry shares validated bytes without evicting live pins. Existing
phase-B files acquire the
auxiliary tables/indexes without rewriting plain message content. The completed
compression pass is cached until its boundary, protected view or eligibility
settings change. With a stable policy, an advancing hot-tail cutoff processes
only the newly cold range. Earlier size qualification survives compression's
own size reduction, so ordinary idle steps do not rescan old rows.

`for_each_message_range` decodes one row at a time. Export, reindexing, search
snippets and history tools use bounded ranges rather than loading every body.
The runtime holds the saved prefix and its tail. Context planning may retain
scalar metadata, token counts and IDs proportional to message count, but hydrates
only selected tail/episode ranges and request anchors. Call/result references
have small durable indexes alongside role, typed-origin and timestamp headers,
so pending-round validation, planning and anchor lookup do not force a cold-body
scan. Imported summaries seed coverage without creating native paid summary
records. Original dangling source call IDs remain in the exact history and
no-reuse registry; the source's compaction closes their earlier serving scope.
New native pending calls still block an incomplete checkpoint. Classical typed-user references preserve the
existing text budget and original IDs. Compression does not change provider bytes.

### Search and serving transactions

FTS5 uses `content=''` and `contentless_delete=1` in `serving.db`. It stores terms
and `(session, seq, message id)` references, without another text copy. Canonical
writes/imports index text before compression; hits hydrate only selected rows for
snippets. Deleted chat rows are removed through the contentless delete protocol.
A wipe rebuilds incrementally in bounded pages. Already indexed archives remain
packed until a hit is resolved; missing archived index pages open lazily.
`bubblepaw search <query> [--chat <id>]` completes missing pages before querying.
Reindexing decodes one message at a time and buffers row savepoints, flushing
once per successful bounded rebuild call rather than per row. This bounds dirty
index pages as well as decoded payloads. Startup checks canonical sequence maxima and invalidates
stale completed-index markers after a lost buffered append. Archived files use
a FULL catalog transition receipt, so this check leaves known archives packed;
missing legacy receipts are repaired only when the reindex budget permits opening.

Serving uses `synchronous=NORMAL`, a buffered transaction and operation savepoints.
Catalog activity updates share its attached projection transaction. Subscribers
receive events immediately from memory; the projections flush at a step/idle
boundary. Graceful close flushes; an abrupt exit may lose disposable journal
references and requires resnapshotting. Durable sequence reservations prevent
old cursors from naming new events after restart, including a serving wipe; see
[session-api](session-api.md). The two projection WALs are independently
rebuildable; their attached commit is not an atomic cross-WAL durability promise.

Canonical chat transactions remain `synchronous=FULL`. Resolved operational
metadata moves out in the transaction that resolves it. For a single native tool
with an existing Allow decision, the assistant call and dispatched effect marker
share a FULL commit before invocation. Ask/Auto/Deny, scripts and parallel calls
retain their own paths. An unknown interrupted external effect is never replayed.

Routine turn completion keeps housekeeping and the buffered flush, but truncates
a WAL only when it reaches the existing 1 MiB journal budget. This amortizes
physical checkpoints without changing FULL record commits. Explicit
`idle_checkpoint` still force-truncates all WALs. Finalization releases the
admission mutex for housekeeping; coordinated admission waits until maintenance
finishes, so idle archiving cannot reenter that mutex and late prompts cannot
attach to an already completed run.

### Phase C verification and measurements

Final Linux release measurements (2026-10-12 UTC / 2026-10-13 local):

| Fixture | Plain MiB | Row-compressed MiB | Archived MiB | Index MiB | Index / text |
|---|---:|---:|---:|---:|---:|
| Codex isaacnet | 199.52 | 189.91 | 69.68 | 58.31 | 33.2% |
| Claude long | 21.23 | 20.91 | 7.52 | 6.27 | 75.3% |

| Fixture | Import s | Peak RSS MiB | Archive open s | Full history s | Next request ms | Search ms |
|---|---:|---:|---:|---:|---:|---:|
| Codex isaacnet | 36.00 | 75.35 | 1.734 | 2.105 | 1.77 | 18.14 |
| Claude long | 2.53 | 19.88 | 0.219 | 0.127 | 2.02 | 3.69 |

Plain rows still use phase A's compressed blobs. Full history is a bounded
decoded scan; next-request preparation uses the source summary plus canonical
tail, with no tools or provider call. Request/search columns use row-compressed
chats. Index size counts FTS/reference/progress pages without another text column.
Search timing is `error` with up to 50 hydrated snippets.

Final gates: fmt check, strict workspace Clippy and 636 workspace tests passed;
strict web Clippy and 12 local web tests also passed. The env-enabled release
private acceptance test passed on both complete files, independently checking
counts and normalized message/derived export hashes. Fixture measurements verify
unchanged request bytes and complete query hit sets across compression, archives
and a serving wipe/rebuild, then zero hits after deletion.

The native-tool turn now makes five application commits (four FULL record commits
and one attached NORMAL projection commit). Its final five-trial median is
5.585 ms, compared with phase A's 5.005 ms and phase B's 11.355 ms. Commits meet
the target; latency remains about 12% above phase A. Routine idle amortizes
checkpoints: native trials retain 1,116,616 total WAL bytes across three files,
each below the existing 1 MiB budget; explicit truncation and bounded 36-turn
growth are tested. Timing samples are diagnostic, not guarantees.

The phase C acceptance run recorded final fixture sizes/timings, native-turn commit counts, exact checks and
verification limits. Both private fixtures stay on Linux; the committed
generator covers their native shapes using synthetic data. No fixture content,
provider calls or generated summaries are part of this acceptance work.

### Measurements with reasoning artifacts kept (2026-10-13)

The phase C numbers above were taken while the importer still dropped
reasoning ciphertext. On 2026-10-13 the Codex fixture was re-staged so that the
secret scrub skips opaque fields: they are byte-identical to the original, and
detectors find 0 hits outside them. The Claude fixture's opaque fields were
unchanged, so its staged file was kept; the reference is
`raw/claude-staged-original.jsonl`. An independent check recomputed the staged
file hashes and opaque-field digests and matched the recorded proof.

Importer v2 keeps every artifact. Source counts equal stored counts: Codex has
25,722 reasoning items and Claude 1,721 thinking blocks; neither fixture has
`redacted_thinking`. Every artifact matches its source in order in plain rows,
row-compressed rows and export. Byte hashes match on sampled artifacts: 101 of
101 for Codex and 102 of 102 for Claude.

| Fixture | Plain MiB | Row-compressed MiB | Archived MiB | Index MiB | Index / text |
|---|---:|---:|---:|---:|---:|
| Codex isaacnet | 302.98 | 292.05 | 124.63 | 64.05 | 36.5% |
| Claude long | 21.93 | 21.66 | 8.26 | 6.54 | 78.6% |

| Fixture | Import s | Peak RSS MiB | Archive open s | Full history s | Next request ms | Search ms |
|---|---:|---:|---:|---:|---:|---:|
| Codex isaacnet | 38.05 | 29.41 | 1.018 | 2.966 | 2.16 | 33.57 |
| Claude long | 2.30 | 20.50 | 0.085 | 0.168 | 2.56 | 3.69 |

Stored bytes by type, in MB. Raw is decoded text; stored counts the bytes the
rows and blobs occupy in the file:

| Type | Codex raw | Codex plain | Codex row-compressed | Claude raw | Claude plain | Claude row-compressed |
|---|---:|---:|---:|---:|---:|---:|
| Tool outputs | 149.2 | 49.3 | 38.6 | 6.00 | 3.80 | 3.07 |
| Tool arguments | 28.3 | 28.3 | 13.6 | 1.71 | 1.71 | 1.06 |
| Reasoning artifacts | 73.9 | 68.2 | 59.9 | 4.05 | 3.84 | 3.42 |
| Plain text | 6.3 | 4.7 | 3.8 | 1.00 | 0.96 | 0.73 |
| Summaries and derived | 3.3 | 3.3 | 3.3 | 1.46 | 1.46 | 1.46 |
| Other row data | 50.7 | 72.2 | 54.1 | 3.13 | 5.01 | 4.12 |
| Page slack and indexes | – | 91.7 | 132.9 | – | 6.21 | 8.86 |

Codex ciphertext barely compresses, so it accounts for most of the growth over
the earlier table: about 60 MB stored and 55 MB archived. The row-compressed Codex
`messages` table holds about 90 MB of unused space inside its pages, with no free
pages; nothing was vacuumed. The Codex search and full-history timings rose;
the peak RSS drop was measured but not explained. Single runs; timings are
diagnostic.

## Phase A implementation

Phase A retained the existing database file. Its schema 7 record formats remain
the basis of the phase B chat files. Phase A introduced the following shapes:

- `messages.message_id` has a nonunique `(session_id, message_id)` index. The
  migration backfills this column without rewriting old message JSON or imposing
  a new uniqueness constraint.
- `events.data` carries content references. Direct tool events reference their
  assistant/tool messages; terminal events reference a run whose
  `result_message_id` identifies the reply (or script result). Inputs, outputs
  and approvals also have references. Readers hydrate the original wire bodies.
  Nested tool progress is transient. State and its events commit together under
  the journal lock, then broadcast in sequence order.
- Pending input text is record content: `inputs.content_blob` and
  `content_format` reference it instead of keeping an operational text copy.
  Consumption transfers the same reference to the message and clears the queue
  reference. Pending input APIs hydrate it; consumed queue text stays empty.
- `provider_state` uses ordered `message-references-v1` metadata: text byte
  ranges and tool indices replace repeated text, names, IDs and arguments.
  Thinking, signatures and opaque provider metadata stay verbatim. Noncanonical
  Responses argument spelling retains a lexical layout for exact request replay.
  Old full-content provider state remains accepted.
- `context_views` stores canonical sequence ranges and rendered text segments
  referencing immutable summaries, episodes, chapters and source-message byte
  ranges. Template text and synthetic metadata remain small literals. Frozen
  checkpoint versions reference summary blobs, so replacing the current rolling
  checkpoint cannot change an already saved view. Reads rebuild `ContextView`.
- `blobs(hash, raw_bytes, data)` stores SHA-256 keyed, zstd level-3 content.
  Tool results over 4 KiB and large inline attachment text use references.
  Error tool results also use blobs below the threshold so a failed script's
  run error can reference the same payload. Run metadata hydrates errors while
  retaining the existing metadata-only `text: null` read contract.
  `attachments` records session ownership, host metadata and original-file blob
  references. The web host persists uploaded originals over 4 KiB; existing
  filesystem paths remain caches and legacy reads still work. Inline attachment
  text shares its original-file blob, including exact JSON escaping on read.
  Reads rematerialize missing filesystem paths for tools that use the emitted
  attachment path; modified or redirected cache files remain rejected.
- The runtime retains parsed history and fetches only the indexed sequence tail.
  Request-byte sizing caches each message's serialized length and serializes a
  message-free request shell. Same-ID context transformations invalidate lengths.
  The O200k token estimator caches content fingerprints (65,536 FIFO entries);
  hits scan bytes for mutation detection but do not serialize or tokenize messages.
- Accepted input/start, assistant-call append, effect dispatch, tool completion
  and terminal reply/state/events each use one transaction per step. Approvals
  and emitted outputs also batch their journal entry with the owning write.
  An effect commit remains synchronous and precedes invocation.
- `synchronous=FULL`, WAL autocheckpoint at 256 pages and
  `journal_size_limit=1048576` remain/set on the record. The runtime attempts a
  truncate checkpoint before publishing idle. Busy readers may defer it. There
  is no automatic VACUUM or orphan-blob collection in phase A.

Legacy messages, provider state and events remain in their original representation.
This phase removed redundancy from new writes; it did not shrink existing
content rows. The following phase implements the physical separation.

## Phase A verification and measurements (2026-10-12)

All Rust commands ran on Linux with toolchain 1.97.1. Required gates passed:
`cargo +1.97.1 fmt --check`,
`cargo +1.97.1 clippy --workspace --all-targets -- -D warnings`, and
`cargo +1.97.1 test --workspace` (549 passed, 0 failed, 9 ignored).
Strict Clippy with `--features web` also passed, along with 12 web attachment and
transport tests. Release runner and scripted-turn example builds passed.

Behavior checks cover reference-only journal rows with unchanged hydrated direct
event bodies, snapshots, legacy/new resume and injected journal failures before
and after an external effect. Messages and Responses complete serialized request
bodies match legacy state byte-for-byte with signed thinking, opaque reasoning,
mixed text/tool blocks and noncanonical argument spelling. Pointer views preserve
the exact prepared ModelRequest across compaction and a physical Store close and
reopen; same-ID signed-zero projections preserve their exact JSON bytes.

An exactly 100 KiB tool output and a duplicate tool output share one
compressed blob and round-trip through history, snapshot, indexed tail, fork and
reopen. Tests also cover the 4 KiB boundary, attachment cache restoration and tag
literals, accepted input references, small/large failed-script error sharing,
real schema 1–6 fixtures, exact cached token estimates, indexed runtime tail
appends, request byte-cap sizing, transaction counts and idle WAL truncation.

The matched dry-run Needle prefix uses `BUBBLEPAW_RECALL_DIR`, the first 2,492
original messages plus the existing neutral archive-gap marker (2,493 canonical
messages), and a 256,000-token threshold. Both versions completed eleven
compactions with zero failures. Recorded tools were inert and summaries synthetic;
there were no paid provider calls. Before source was frozen at
`953926c0f057129a55498beed4e3b65118669ffe`.

| Metric | Before (schema 6) | Phase A (schema 7) |
|---|---:|---:|
| Retained database bytes after exit | 7,974,912 | 4,706,304 |
| Runtime database bytes | 7,446,528 | 4,460,544 |
| Runtime WAL bytes | 1,433,792 | 1,048,576 |
| Sampled WAL peak bytes (5 ms) | 1,433,792 | 1,194,832 |
| Replay transactions | 110 | 110 |
| Canonical import / context transactions | 65 / 45 | 65 / 45 |
| Replay wall seconds | 12.7832 | 2.4608 |
| Process wall seconds | 12.9674 | 2.5693 |
| Summary / decision calls (synthetic) | 33 / 15 | 33 / 15 |
| Input / output tokens | 368,548 / 10,307 | 368,548 / 10,307 |
| Scripted native-tool turn transactions | 12 | 5 |
| Scripted native-tool turn wall seconds | 0.006600 | 0.005005 |
| Scripted turn WAL bytes on return | 659,232 | 0 |

This pair reduced retained size by 41.0% and replay wall time by 5.19 times.
It is one timing pair, not a latency distribution or live-model quality result.
The sampled WAL peak is a lower bound and can miss shorter spikes;
`journal_size_limit` limits retained journal size rather than every transient peak.
Transaction counts exclude schema initialization and count successful commits.
Replay exercises import/context work, so its transaction count is unchanged.
The separate scripted provider turn executes one native effect tool and a final
reply; schema 6 counts its seven Store commits plus five legacy event autocommits,
while schema 7 includes journal writes in its five Store commits. Its tiny timings
are diagnostic only.

All eleven rendered views match byte-for-byte after normalizing only declared
generated chapter/episode/session IDs and generated message headers. Frontier,
masking and clipping choices and all 48 call schedules/token estimates match.
Raw rendered-view token counts differ in seven compactions, by at most nine
tokens, because UUID text itself is tokenized. An unchanged-baseline repeat
also differed in eight compactions, by up to eighteen tokens. These raw differences
are retained, not normalized away; exact same-input estimator and request-body
equivalence are verified separately by Rust tests. No live-model or vendor
signature acceptance is claimed.

`dbstat` table page bytes (including overflow pages; excluding indexes):

| Table | Before bytes | Phase A bytes |
|---|---:|---:|
| approval_rules | 4,096 | 4,096 |
| approvals | 4,096 | 4,096 |
| attachments | — | 4,096 |
| blobs | — | 1,306,624 |
| context_chapters | 4,096 | 4,096 |
| context_checkpoint_versions | — | 4,096 |
| context_checkpoints | 4,096 | 4,096 |
| context_decisions | 12,288 | 12,288 |
| context_episodes | 73,728 | 73,728 |
| context_subject_chapters | 4,096 | 4,096 |
| context_views | 32,768 | 12,288 |
| effects | 4,096 | 4,096 |
| events | 4,096 | 4,096 |
| inputs | 4,096 | 4,096 |
| messages | 7,405,568 | 2,809,856 |
| metadata | 4,096 | 4,096 |
| outputs | 4,096 | 4,096 |
| runs | 4,096 | 4,096 |
| sessions | 4,096 | 4,096 |
| sqlite_sequence | 4,096 | 4,096 |

The replay-only message-ID index (139,264 bytes) is replaced by the production
`messages_id` index of the same size; `messages_session` remains 135,168 bytes.
The blob primary-key index occupies 24,576 bytes for 245 distinct blobs. Every
table/index allocation and row count is retained in the JSON artifacts. Empty
tables still own a 4,096-byte root page, so this replay does not measure event
content savings; the scripted-turn and reference-row tests establish those.
