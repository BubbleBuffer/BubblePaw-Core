# Session API: several clients on one session

Status: built; current as of 2026-10-29.

Several clients (a desktop, a phone, a script) can watch and drive one session
at once. Each loads a snapshot, then follows one event stream from the snapshot's
position. Nothing is missed or applied twice, and every client sees which client
started, steered, cancelled or approved what.

## Shared implementation

All host commands are entries in the [host API table](host-api.md) in the runner
library. stdio, HTTP `POST /rpc`, REST routes, CLI session commands and SDK callers
invoke that table; all local adapters use `Profile::Full`. Protocol 0.1 names and
shapes remain compatible. Capability advertising comes from the table. The CLI
now uses the same delete/cancel cascades and includes usage in session listings.

## Stdio host controls (2026-10-29)

`initialize.features` advertises `providers.list`, `roles.set_main`,
`session.list.pagination` and `child.cancel`; the protocol remains `0.1`.
These methods are available in ordinary stdio builds, including builds without
the optional web feature.

| Method | Params | Result |
|---|---|---|
| `providers.list` | None | The same redacted setup snapshot as `GET /api/providers`: configured providers/models, provenance and target file, `main`, `active_main`, `launched_main`, `reload_needed` and `restart_needed`. Credential values are omitted; `key_present` reports availability. |
| `roles.set_main` | `model: "provider/model"` | The updated provider setup snapshot, using the same validation, configuration layering, atomic file replacement and live apply as `PUT /api/roles/main`. The provider must exist. Later-layer overrides reject the write. No model request or session is created. |
| `session.list` | Optional `limit`, `cursor` | Without either parameter, the existing session array and ordering. With either parameter, `{sessions, next_cursor}`; the last page has `next_cursor: null`. Hidden sessions are excluded and each entry retains its `usage` field. |
| `child.cancel` | `parent`, `child` (session IDs) | The ordinary child cancellation result (`sessionId`, `handle`, `status`, and available result fields). Cancels the addressed child and its subtree and awaits cleanup through the existing child runtime. Both IDs must belong to this workspace, and `parent` must be the direct parent. |

Paged lists accept an integer `limit` from 1 through 1000 (default 100 when
only `cursor` is supplied). Omit `cursor` or send `null` for the first page;
pass the returned string unchanged for the next page. Invalid limits and cursors
are errors. Paged ordering is immutable `created_at` descending, then session ID
descending, so renames and new activity do not move entries between pages. The
cursor is URL-safe unpadded base64 of versioned JSON containing `version: 1`,
`created_at` and `id`. It remains usable after the boundary session is deleted.
Pagination reads current metadata, not a frozen snapshot: concurrent creation or
deletion can change membership. It bounds returned entries and usage reads;
the existing store still loads visible session metadata to order the page.

Child cancellation keeps ordinary direct-parent ownership, restored-child binding,
question cancellation, descendant cleanup and unknown-effect rules. It creates
no new cancellation mechanism and never resumes or replays an effect. Root/run
cancellation remains `run.cancel`.

## Context reads and compaction notifications (2026-10-27)

Stdio `initialize` advertises `"context"` in `features`; `protocol_version`
remains `"0.1"`. Clients can use that feature to detect all five reads below.
Each requires `session_id` and checks that the session belongs to this host's
workspace. Reads do not gather, apply, update a view or write token estimates.

| Method | Additional params | Result |
|---|---|---|
| `context.view` | None | `{view: ContextView \| null}`; null before a view is saved. |
| `context.structure` | None | `{episodes: [...], chapters: [...], legacy_chapters: [...], summary_tokens_estimated: true}`; metadata without full summaries. |
| `context.summary` | `kind: "episode" \| "chapter"`, `id` | `{kind, id, summary}`; full summary text, or null for an open chapter without a summary. Unknown IDs are errors. |
| `context.events` | None | `{events: [...]}` in stable chronological insertion order. |
| `history.range` | An inclusive `first_seq`/`last_seq` pair **or** `first_message_id`/`last_message_id` pair; optional `limit` (default 200, maximum 1000) | `{messages: [{seq, message, tokens, source}], next_seq}`; canonical originals in sequence order. `next_seq` is the next row within the requested range, or null. |

Sequence bounds must be nonnegative and ascending. Do not mix sequence and ID
bounds. ID bounds must belong to the requested session. A subsequent page can
use `next_seq` as `first_seq` while retaining the original upper sequence bound.
Message token `source` is `"reported"` or `"estimated"`. Provider-reported zero
is distinct from missing usage. Reported counts come from linked physical
requests; estimates use the runner's existing tokenizer and are never persisted.
The counts-only SDK read decodes only messages needing estimates; `history.range`
also decodes originals to return their bodies.

Web exposes the same payloads through `GET /api/sessions/{id}/context/view`,
`/context/structure`, `/context/summary?kind=episode&id=episode-1`,
`/context/events`, and `/api/sessions/{id}/history/range?first_seq=1&last_seq=100`.
The abbreviated context paths share the `/api/sessions/{id}` prefix. Range
options use query parameters. Invalid parameters return 400, unknown summaries
404, and sessions belonging to another workspace 409.

The saved view includes `frontier`, `prefix`, `tokens`, `blocks_estimated` and
ordered `blocks`. Each block has `kind`, `tokens`, optional `id`, and optional
`first_message_id`/`last_message_id`. Block kinds are `opening`, `memory_index`,
`anchors`, `chapter_summary`, `episode_summary`, and `raw_suffix`. These are
apply-time measurements; appending a raw message does not rewrite the snapshot.
Old views may have no blocks. See [context compaction](context-compaction.md).

Episode metadata includes `id`, `ordinal`, `chapter_id`, covered message IDs,
`title`, `status: "written"`, `source_tokens`, and `summary_tokens`. Subject
chapters include `id`, `ordinal`, first/last episode ordinals, covered message
IDs, `title` (the saved description, nullable), `status` (`open` or `closed`),
`source_tokens`, and `summary_tokens`. Legacy chapter source counts are null.
Summary token counts estimate summary text, rather than the rendered view block.

`context.events` returns the recorded fact, including its `id`, `kind`,
`trigger`, `trigger_text`, source range, episode/summary IDs, `tokens_before`,
`tokens_after`, and optional `input_before`/`input_after`. `tokens_before` is null
when inherited coverage lacks stored source counts; reading or applying never
loads cold originals just to fill that count. Existing stores have no backfilled
events.

When a fact is written, the session event stream emits `context_compaction`:

```json
{"sequence":42,"durable":true,"session_id":"chat-1","run_id":"","created_at":"2026-10-27T11:00:00Z","type":"context_compaction","event_id":"fact-1","kind":"applied","trigger":"pressure"}
```

The payload fields are `event_id`, `kind` (`episode_written`, `chapter_closed`,
or `applied`) and `trigger` (`pressure`, `emergency`, `source_size`,
`subject_change`, or `manual`). Clients re-read the affected Context resources
after this notification. It uses ordinary stdio event notifications,
`events.after`, and the existing web SSE stream.

The fact and notification share the existing canonical write boundary. For a
single-file store the journal row shares that transaction. In the central layout,
the fact is in the chat database and the disposable journal is buffered in
`serving.db`, joining the existing serving/catalog flush. Notifications add no
chat commit and no independent serving commit. The journal's existing gap,
reopen and resnapshot rules still apply; there is no cross-WAL atomicity claim.

Example requests and abbreviated results:

```json
{"jsonrpc":"2.0","id":1,"method":"context.view","params":{"session_id":"chat-1"}}
{"jsonrpc":"2.0","id":1,"result":{"view":null}}
{"jsonrpc":"2.0","id":2,"method":"context.summary","params":{"session_id":"chat-1","kind":"episode","id":"episode-1"}}
{"jsonrpc":"2.0","id":2,"result":{"kind":"episode","id":"episode-1","summary":"Retained summary text."}}
{"jsonrpc":"2.0","id":3,"method":"history.range","params":{"session_id":"chat-1","first_seq":1,"last_seq":100,"limit":20}}
```

## Quota waits (built 2026-10-20)

An exhausted provider account can leave an active run in `waiting_for_quota`.
The durable `run_waiting_for_quota` event carries `wait` with `session_id`,
`run_id`, `account`, `window`, `resume_at` and `reason`. The original execution
future remains active; clients should treat the session as busy and allow
cancellation. `run_quota_resumed` marks clearing the wait before retry dispatch.
Ordinary completion, failure and cancellation still produce `run_finished`.

After process loss during that wait, the old run becomes `interrupted` and its
marker survives. The owning host schedules an ordinary resume at the reset,
producing a new `run_started` and run ID. Counters reset as in today's resume
path. Expected-marker admission is atomic with creating the new run; cancellation,
a newer run or an unknown external effect prevents stale automatic recovery.
One-shot hosts resume only due startup markers and report future waits; persistent
web/stdio hosts schedule future ones. See [quota](quota.md) for the exact policy.

## Panics (built 2026-10-23)

A turn that panics in a provider, tool or host hook settles its run as `failed`
with a `run panicked: ...` error, which `Harness::prompt`, `resume` and
`wait_for_idle` return instead of waiting forever; the session itself stays
usable for a later `prompt`.

## Host commands and session control (built 2026-10-22)

`/rename <title>`, `/rewind <message-id>`, `/compact [focus]` and
`/reload [server]` typed as the whole input of a run, steer or follow-up are host
commands: they never reach the model or start a run. One library executor,
the shared `bubblepaw_runner::host_api` method table (which owns `SessionControl` internally),
runs them for stdio, web and CLI `run`, so each returns the same JSON on every
transport: rename returns the session, rewind the rewind result, compact
`{notice}` and reload `{reload: diff}`. Cancel (`run.cancel`, `POST .../cancel`)
and the workspace binding check go through the same service.

- A command addressed to a session first checks that the session exists and
  belongs to this host's workspace (web: 404 or 409).
- `/reload` is host-scoped: CLI `run '/reload'` creates no session. A host
  without live config answers it with an error (web 400) instead of sending it
  to the model.
- Web only: input with attachments is always a prompt. After any command web
  refreshes session lists and publishes a reset so other clients resnapshot.

## Session names (built 2026-10-15)

Titles are optional on creation: `session.open {title?}` (or `name?`) defaults
to `New session`. CLI `run` and `script` accept `--name <title>` for new
sessions. An explicit name is trimmed, nonempty and at most 256 UTF-8 bytes.
`--name` conflicts with `--session`; changing an existing title uses
`session.rename {session_id, title}` or `/rename <title>` through run, steer or
follow-up commands. The CLI equivalent is
`bubblepaw run '/rename New title' --session <id>`.

HTTP accepts `POST /api/sessions/{id}/rename {title}`; creation also accepts
`name` as an alias for `title`. Successful web renames refresh session lists
and publish a reset so other clients resnapshot. A rename updates metadata,
including `updated_at`, without adding conversation input or calling a model.
Saved titles appear in session snapshots, lists and CLI message-search hits.

## Ephemeral CLI runs (built 2026-10-15)

`run --no-session` and `script --no-session` use a private temporary directory
for the entire store and its spill/output/checkpoint resources. That directory
is selected before opening any configured data directory.
The real store is neither opened nor changed, even when `--data-dir` or `--db`
is supplied. `--session` conflicts with `--no-session`; `--name` may label a
temporary run. Ordinary completion, handled cancellation and errors drop the
temporary directory, retrying for up to five seconds while a file in it is still
briefly open (on Windows a closing handle or a virus scan blocks the removal).
Abrupt process termination cannot run cleanup destructors.
This mode does not change tool access to the working directory.

## Todo and questions (built 2026-10-15)

Both are host capabilities under `tools.*` inside `js`; the model-facing tool
surface remains only `js`.

`tools.todo({items: [{text, status}]})` replaces the entire list and returns it. Status is
`pending`, `in_progress` or `done`; the cap is 50 items, with nonempty text up to
1024 UTF-8 bytes per item. `{items: []}` clears it; a bare array is still accepted
when it reaches the handler, but JavaScript calls are checked against the object
schema, which providers require for native function tools. Each chat has its own latest list,
stored in the authoritative chat file. The tool call stays in ordinary history;
there is no separate prompt injection.

`tools.question({text, options?})` waits for free text; options are suggestions.
A root question appears in `session.state.question` and the web session view.
Stdio clients answer using `question.answer {session_id, question_id, text,
client?}`; the web method is `POST /api/sessions/{id}/question/answer` with
`{question_id, text, client?}`. Clients show a text input and optional
suggested answers. The original tool call returns the answered question object.
One unanswered question is allowed per session. Question and answer text are
bounded to 16 KiB; options to 16 strings of at most 1024 bytes each.

`session.state` and the web `SessionView` expose `todo` (default `[]`) and
`question` (pending object or `null`). A durable `host_state_changed {key,
value}` event carries `key: "todo"` or `key: "question"` for each change,
including question resolution/cancellation. Clients can resnapshot after it.
These events use the existing stdio event notifications and web SSE stream.

Root one-shot `run`, `resume` and `script` return `no_user_available` immediately
without storing a question. Child questions go to their direct parent, which
answers through `tools.agent_send`; the host user can also answer with
`question.answer_child {parent, child, question_id, text}` or
`POST /api/sessions/{parent}/children/{child}/question/answer {question_id, text}`.
Both return `{answered: true}` and require the actual direct parent and exact
question ID. The answer must be nonblank and at most 16 KiB. The original child
binding is retained; see [child sessions](child-sessions.md).
Root-client answer endpoints reject child sessions before changing their state;
use the explicit parent/child endpoint for a host user answer.
Cancel ends a wait, including a pending question restored after restart.

Saved question objects include `asked_at` (RFC 3339; absent on older records).
An answered child's object records `answered_by: "user"` for a host user answer
and `"parent"` for `agent_send`; older answers without it are parent answers.
The child's `host_state_changed {key: "question", value}` event remains the
authoritative question change. The direct parent receives a bounded projection:
`key: "child_question"` for pending/cancelled questions, and the existing
`key: "child_question_answer"` for answered questions. These leave the parent's
own `question` alone. See [child events](#child-events-on-the-direct-parent-stream)
for payloads and transaction boundaries.

Question state survives restart. Answering after loss of the volatile waiter
admits a new generated continuation containing the original question and answer,
with a durable answer marker for retry deduplication. It never reruns the old
JavaScript cell or an unknown external effect. The answer remains in host state
until the next question replaces it; the root and parent answer paths can retry
failed continuation admission with the same question ID and text. The host user
child-answer path requires pending state, so a repeated accepted answer returns
a conflict; recovery delivers a saved answer without accepting a second one.
There is no model context injection
of todo lists.
If a restart lands between saving an answer and admitting its continuation, the
next ordinary turn receives the saved answer once through a durable host notice.
Startup does not make a model call.

Live answers enter the existing durable input queue before the tool resumes;
this also preserves two sequential questions inside one JS cell. Accepted answer
input is consumed as a canonical generated message with its receipt marker.
Script-only runs consume host-generated steering after their tool result and
finish without starting a model; explicit user steering/followups keep their
existing behavior.

### Steering request boundary (built 2026-10-18)

Verified by local core regressions. Live model acceptance has not run for this change.

`run.steer` accepts input into the durable ordered queue and records the sending
client immediately, including while a model, tool or context preparation is
active. It does not cancel generation, release foreground waits or force an
extra generation just to accept input. Steers accepted during asynchronous
preparation are drained in order, then the imminent request is prepared again
from fresh canonical history so token/window admission and usage calibration
include them. A fitting request does not need a new auxiliary model call; new
pressure still follows ordinary context policy. The new instructions retain
their exact canonical user-message origin. Followups and other
sessions remain separate. A later ordinary request includes those messages once
in its history. The final-response pending-input check still prevents silently
settling with an unhandled steer.

The snapshot cutoff is the final queue drain before main provider dispatch.
Input accepted after that cutoff waits for the next outer request boundary.
Provider-owned HTTP retries and automatic length continuations reuse their
captured request; they do not drain new input midway through partial assistant
state. LCM and other auxiliary requests never receive this main-request drain.
Explicit host wait-release, background and cancel controls remain available.

Context admission and main dispatch use the same captured provider settings.
A reload during asynchronous preparation applies to a later request, including
when steering causes fresh preparation of the current request. Manual
compaction also captures one provider snapshot. Bound preparation shares the
original context's calibration and token caches; the usage wrapper forwards
that snapshot and runtime-inventory notices to the selected context policy.

The engine provides a generic bounded host-state row/change event and the small
script-boundary handling of host-generated input;
tool semantics and question routing stay in the runner. State updates commit
with their event under the snapshot lock. No existing schema is changed.

Surfaces: stdio has `session.state`, `events.after` and `events.gap`, which
the stdio loop (`run_stdio` in `main.rs`) forwards as notifications. The web
host has `GET /api/sessions/:id` and the `GET /api/events` SSE stream with
`Last-Event-ID` replay and `reset` (the router in `web.rs`, and the handler
`events_stream`). Core has `snapshot_with`, `events_after` and `can_resume_from`
in `event_log.rs`.

## Snapshot plus follow

1. Load the session's state. It carries `seq`, the last durable event it reflects.
2. Follow events with a sequence above `seq`.

The chat file commits the state change before the buffered serving journal is
flushed. There is no atomic transaction across the record and projections. A snapshot may therefore already
reflect a change whose event has not been journaled yet; apply events idempotently,
by message index and run id. Journal failure cannot undo an accepted input or
cause an external effect to run again. Clients resnapshot when replay reports a gap.

- stdio: `session.state { session_id }` returns the state plus `seq` and
  `streaming_text`, then `event` notifications follow.
- web: `GET /api/sessions/:id` returns a `SessionView` with `seq` and
  `live_seen`; `GET /api/events` is the SSE stream ([web contract](web-contract.md)).
- core: `Harness::snapshot_with(session_id, read)` runs `read` (store reads only)
  and returns its result with a `LogPosition { seq, streaming_text, live_seen }`;
  `Harness::subscribe()` follows.

## Explicit compaction

`Harness::compact_session(session_id, focus: Option<&str>)` asks the host's context
policy to compact an idle session and returns an optional human-readable notice.
Ordinary full-history hosts report that manual compaction is not configured.
The runner supports LCM sessions through stdio `session.compact` with
`{session_id, focus?}` (`focus` must be a string or null). `/compact [focus]` is
also intercepted by CLI `run --session`, stdio message commands and web message
routes. The response is `{notice: ...}` instead of a new run or queued input.

The runner bypasses automatic triggers, including `--no-auto-summarize`, but retains
original messages. A short session with no older complete span returns an explicit
no-op notice. Busy sessions reject the command; start, fork and delete cannot race
a compaction. `Harness::cancel` and concurrent HTTP cancel requests cancel compaction.
The stdio dispatcher handles commands serially, so a later `run.cancel` RPC waits
for the compaction response; the existing summary timeout bounds that wait.
These commands do not create
a user message, a main-model turn, or a new model-facing tool. See
the manual compaction section of the context episodes doc for focus scope, retained
tail and failure behavior.

## File rewind (2026-10-15)

`session.rewind {session_id, message_id}` forks through that canonical message
and restores only that session's later checkpointed file changes whose current
post-images still match. It returns a new `session_id`, `restored`, `deleted`,
`recreated`, `conflicts` and the file-tool-only `limitation`. The original chat
history remains unchanged. `/rewind <message-id>` is intercepted on the same
CLI, stdio and web message surfaces as `/compact`; HTTP also exposes
`POST /api/sessions/{id}/rewind {message_id}`. There is no model-facing rewind tool
and no force option. See [checkpoints and rewind](rewind.md) for bounds, fork
restrictions, conflict behavior and retention.

The fork's `todo` is the list recorded at that message, independent of the source's
latest list, and its `question` is `null`. A pending question remains answerable
only in its original session. Todo updates inside a tool cell become eligible
after the completed result; historical values predating snapshot support cannot
be recovered and default to `[]`.

## Durable and live events

All event kinds are durable except `paragraph`, `text_delta`,
`model_request_started`, and nested host tool progress (`tool_started` and
`tool_finished` emitted inside a tool cell). The transient kinds are listed under
[event kinds](#event-kinds).
Direct model tool boundaries remain durable in the chat record. Journaled events
receive increasing sequence numbers and are broadcast immediately from memory.
Their disposable SQLite references are buffered until a turn step or idle flush,
using `synchronous=NORMAL`. An abrupt exit can lose the buffered journal tail;
accepted input, canonical messages and effect boundaries retain their FULL
record durability. Replay then requires a fresh snapshot.

Startup reserves a sequence range in the catalog and skips the previous range
on restart. Sequence gaps are intentional: an old client cursor cannot name a
new event after a crash or serving wipe. Numbers remain exactly representable by
JavaScript. A surviving serving receipt backs up the catalog reservation; if both
projections disappear, existing chat files trigger a new time-based epoch.

The disposable SQLite journal stores references to messages, inputs, outputs, runs and
approvals, plus small status metadata. `events.after` and the web replay hydrate
those references into the same complete event bodies subscribers receive.
Legacy full-content journal rows remain readable without rewriting them.
Snapshots keep their existing message and run-metadata shapes. A completed
run's return value resolves its result-message reference while its operational
row is retained; an unknown external effect is still never replayed on resume.
Finished run metadata and journal replay may disappear after retention or a wipe.
Messages, accepted inputs, pending work and effect boundaries remain in chat files.

Nested tool progress is live-only, carries the last durable sequence, and is
neither replayed nor counted in `live_seen`. Its parent tool result and any
external effect boundary retain their normal durable record.

A `text_delta` is live progress: one provider text batch as it streams. It is
`durable: false` and not written, carries the same live `sequence` rule as a
paragraph, and is neither in `streaming_text` nor counted in `live_seen`. Hosts
may show it as provisional text; the reply itself is the paragraphs and the
stored message.

A `paragraph` is live-only: `durable: false`, not written, and its `sequence` is
the last durable sequence before it, so several paragraphs share one number.
Paragraphs between a snapshot and its follow are told apart by `live_seen`: how
many live events of that session carrying `seq` the snapshot already includes.
Skip that many, apply the rest. `streaming_text` holds the paragraphs of the reply
still being written (joined by blank lines, capped at 1 MiB); it clears once the
reply is stored as a message.

One store has one sequence and one stream, shared by every harness on it,
including child agents. Child events retain their own session IDs. The bounded
parent events described below also appear when a client follows only the direct
parent's session ID.

### Child events on the direct parent stream

Stdio `initialize.features` includes `child_events`. The existing `event`
notifications, `events.after {session_id: parent, after}` and web `/api/events`
SSE deliver these durable envelopes. A parent event's `session_id` is the actual
direct parent; clients need no descendant filter or new subscription endpoint.
Run-state projections skip hidden/internal children. Question projections retain
the existing direct-parent question behavior, including hidden worker parents.
A grandchild projects to its immediate parent, without duplicating the event on
an ancestor's stream.

`child_run_state_changed {child_session_id, state}` uses the envelope's `run_id`
for the child run. Its `state` is `running` at durable run admission or the actual
stored terminal run status at durable completion (`completed`, `cancelled`,
`failed` or recovery's `interrupted`). For example:

```json
{"sequence":42,"durable":true,"session_id":"parent-session-id","run_id":"child-run-id","created_at":"2026-10-27T12:00:00Z","type":"child_run_state_changed","child_session_id":"child-session-id","state":"completed"}
```

Question projections use the existing `host_state_changed` kind. Their `value`
has `child_session_id`, `id`, `parent_session_id`, `status`, `text`, `options`,
`asked_at` and `run_id`; `asked_at` is `null` for older questions without a
timestamp. Answers add `answer` and `answered_by: "parent" | "user"`.
The transient delivery bookkeeping field is omitted. A pending kind payload is:

```json
{"type":"host_state_changed","key":"child_question","value":{"child_session_id":"child-session-id","id":"question-id","parent_session_id":"parent-session-id","status":"pending","text":"Which colour?","options":["Blue","Green"],"asked_at":"2026-10-27T12:00:00Z","run_id":"child-run-id"}}
```

Cancellation uses the same key with `status: "cancelled"`; either author answering
uses `child_question_answer` with `status: "answered"`, `answer` and `answered_by`.
Each key retains only its latest value; the durable event sequence describes the
transitions. These parent values are event hints, not a pending-question inventory.
Clients derive `waiting_for_answer` from the child's authoritative pending
question and resnapshot that child after an event or stream reset.
Run events do not report quota waits, process-local registry `idle` states or
cleanup progress. They cover only the existing durable admission and terminal
boundaries, so a runner's `timed_out` result remains distinguishable from the
underlying stored run status.

For parent and child in the same chat database, each projection shares the
existing state-change transaction and serving-journal batch. A question whose
parent is in another database may require one additional parent transaction.
Ordinary parent turns add no child-event transaction. These are durable status
projections, without stream fragments or a separately persisted registry.

Startup recovery also projects `interrupted` for recovered visible child runs.
Single-file stores commit the canonical interruption and its parent projection
atomically, without an additional transaction. Directory stores first finish the
canonical interruption, then publish its parent event after catalog and sequence
initialization; this adds one transaction per recovered visible child run. There
is no durable outbox between those steps. A failed directory reopen or crash
before the queued projection is flushed can require reset/resnapshot; the next
startup's sequence reservation invalidates old cursors. This contract covers
normal chat-v1 directory stores, without promising parent-event replay across store layouts. Registry-only idle and cleanup states
remain outside the event contract.

### Event kinds

Every event is a `HarnessEvent`: `sequence`, `durable`, `origin` (when a client
caused it), `session_id`, `run_id`, `created_at`, and the kind's own fields under a
`type` tag (`EventKind` in `lib.rs`). These are all the kinds.

Durable kinds are journaled and take sequence numbers:

- `run_started {model}`: a run was admitted.
- `child_run_state_changed {child_session_id, state}`: a visible direct child's
  durable run admission or terminal boundary projected onto its parent's envelope;
  `run_id` identifies that child run. See [child events](#child-events-on-the-direct-parent-stream).
- `run_waiting_for_quota {wait}` and `run_quota_resumed {}`: see Quota waits.
- `tool_started {call}` and `tool_finished {call_id, name, result}`: direct model
  tool boundaries. Nested host tool progress inside a tool cell is transient.
- `steering_accepted {text}` and `followup_queued {text}`: input accepted by
  `run.steer` or `run.follow_up`.
- `output {record}`: a durable `OutputRecord` from CodeMode.
- `host_state_changed {key, value}`: a host state value changed, such as `todo`,
  `question` or `usage.last_request`.
- `approval_requested {approval}` and `approval_resolved {approval_id, state, reason?}`.
- `run_finished {status, text?, error?, usage?}`: the run's terminal status.
- `session_settled {}`: published once a run has finished and the session has
  nothing left to run. It never carries `origin`.

Transient kinds have `durable: false`, take the sequence of the last durable event
before them, are not journaled and are not replayed:

- `paragraph {text}`: one paragraph of the reply being written. It is also in `streaming_text`.
- `text_delta {text}`: one provider text batch as it streams.
- `model_request_started {}`: a provider request is about to be sent, including
  correction retries. Hosts can reset partial response capture on it.

## Origin

`run.start`, `run.resume`, `run.steer`, `run.follow_up`, `run.cancel`,
`approval.decide` and `question.answer` (and the matching web requests) take an optional
`client: { id, name? }` (id 1 to 256 bytes, name up to 256). The event the
command causes carries it as `origin`: `run_started`, `steering_accepted`,
`followup_queued`, `approval_resolved`, and `run_finished` when a client's cancel
ends the run. `session_settled` never carries `origin`. A run continued from a
queued follow-up has no origin. Commands without `client` produce events without `origin`.

A run on a busy session fails with
`Session already has an active run <run_id>; steer or follow up instead`.

## Stdio methods

The stdio API is JSON-RPC over the runner's stdin and stdout (`run_stdio` in
`main.rs`). Session methods take `session_id` in params. Each command returns the
result shown; errors are JSON-RPC errors. Host commands typed as the text of
`run.start`, `run.steer` or `run.follow_up` run as those commands
([host commands](#host-commands-and-session-control-built-2026-10-22)).

- `initialize` returns `{protocol_version: "0.1", features, transport: "stdio", input_limits}`.
  `features` includes `question` for root question support and
  `question.answer_child` for the explicit host user child-answer method. Clients
  can check that exact feature string before offering child answers. `child_events`
  advertises durable run/question projections on the direct parent's stream.
- `shutdown` returns `{stopped: true}` and ends the loop after the reply.
- `config.reload {server?}`: the reload diff.
- `session.open {title? | name?, session_id?, parent_session_id? | parent_id?}`: a
  session. With `session_id` it loads that session; otherwise it creates one, with
  the title defaulting to `New session`.
- `session.list`: sessions, each with its `usage` projection.
- `session.rename {session_id, title}`: the session.
- `session.rewind {session_id, message_id}`: the rewind result.
- `session.compact {session_id, focus?}`: `{notice}`.
- `session.delete {session_id}`: `{deleted, worktrees}`.
- `session.state {session_id}`: the session, `messages`, `runs`, `inputs`, `outputs`,
  `pending_effects`, `todo`, `question`, `usage`, `repl`, plus `seq` and
  `streaming_text`. Unlike the web view, `messages` are returned as stored, so
  `provider_state` appears when a message has one.
- `events.after {after?, session_id?, limit?}`: `{events, last_seq, resync}`, at most
  1000 events per call (see Catching up).
- `environment.inspect {session_id}` and `environment.control {session_id, control}`
  (`control` defaults to `{action: "inspect"}`).
- `run.start {session_id, text, options?, script?, client?}`: the admitted run.
- `run.resume {session_id, options?, client?}`: the admitted run.
- `run.steer {session_id, text, client?}` and `run.follow_up {session_id, text, client?}`:
  the queued input.
- `run.cancel {session_id, client?}`: `{requested}`.
- `process.list {session_id}` and `process.background {session_id, handle}`: the
  process jobs, and the backgrounded process snapshot.
- `approval.list {session_id}`: pending approvals.
- `approval.decide {approval_id, decision, reason?, client?}`: the decided approval.
- `question.answer {session_id, question_id, text, client?}`: `{question, live}`.
  This method rejects child sessions.
- `question.answer_child {parent, child, question_id, text}`: `{answered: true}`.
  The host user answers a direct child's exact pending question. Both sessions
  must belong to this host's workspace. Unknown children, wrong direct parents
  and unknown question IDs fail; an already answered/cancelled question conflicts.
  Blank or oversized text fails. Errors use the ordinary JSON-RPC error envelope.
- `storage.stats`: storage statistics.

Notifications from the host: `event` (a `HarnessEvent`) and `events.gap {skipped,
after, recover_with: "events.after"}`.

### Child-answer request and response

After reading a child's pending `question` from `session.state`, send its exact
`id` with the direct parent and child session IDs. All four fields are required;
this method uses `parent` and `child` instead of `session_id`. It has no client
attribution argument:

```json
{"jsonrpc":"2.0","id":17,"method":"question.answer_child","params":{"parent":"parent-session-id","child":"child-session-id","question_id":"pending-question-id","text":"Green with stripes"}}
```

Successful live delivery or admission of a safe child continuation returns:

```json
{"jsonrpc":"2.0","id":17,"result":{"answered":true}}
```

An unknown question ID, including an ID superseded by a newer question, returns:

```json
{"jsonrpc":"2.0","id":17,"error":{"code":-32000,"message":"pending child question not found"}}
```

The same error shape and code apply to other command failures. A question already
answered or cancelled reports `question was already answered or cancelled`.
Stdio rejects blank text with `Text or at least one attachment is required`;
answers over 16 KiB that fit the host's prompt limit report
`answer must be nonempty and at most 16 KiB`. The host prompt limit is checked
before question validation. No question state changes on those refusals. The
older `question.answer` method continues to reject child sessions.

## Catching up

A client that missed events asks for the logged events after its last durable
sequence:

- stdio: `events.after { after?, session_id?, limit? }` returns
  `{ events, last_seq, resync }`, at most 1000 events per call; page by passing
  the last returned sequence. `resync: true` means the log no longer holds
  everything after `after` (pruned, or `after` is beyond `last_seq`): reload
  `session.state`. When the stdio transport drops notifications it sends
  `events.gap { skipped, after, recover_with: "events.after" }`. Without `session_id`,
`events.after` drops events of hidden sessions; with `session_id` it does not filter.
- web: reconnect with `Last-Event-ID` (the browser does this itself). The host
  replays the missed durable events, or sends `reset` when it cannot.
- core: `Harness::events_after(after, session_id, limit)`,
  `Harness::can_resume_from(after)`.

Missed paragraphs are not replayed; the stored reply replaces them.

## Retention

Directory stores prune finished-run journal entries after 24 hours and diagnostic
usage/timing telemetry after 30 days, at startup and on idle. Authoritative
per-request usage in the chat file has no time-based retention. The runner configures these through
`storage.finished_event_retention_hours` and `storage.telemetry_retention_days`;
see [configuration](config.md). Pruned replay positions get `resync`/`reset`.
Deleting a session deletes its events. The older
`HarnessConfig::event_retention_days` override remains available to library hosts.

`bubblepaw serving wipe` recreates the disposable store. Hosts also recreate a
missing `serving.db` on open. After either operation, **every session-api client
must discard its replay position, load a fresh snapshot and follow from that
snapshot**. A deleted journal burns the old sequence range; an old cursor
is not a valid replay position in the new journal. The chat history and crash-safe pending
state survive, while frozen context views are rebuilt from retained history and
summaries. This does not preserve an arbitrary old frozen view's selection or
custom prompt.

## Example: desktop and phone

1. Desktop: `session.state` returns `seq: 40`. Phone: `GET /api/sessions/s1`
   returns `seq: 40, live_seen: 0`, and the phone opens `/api/events`.
2. Phone: `POST /api/sessions/s1/run { text, client: { id: "phone" } }`.
   Both receive `run_started` (seq 41, `origin.id: "phone"`), then paragraphs
   with sequence 41.
3. Desktop: `run.steer { session_id: "s1", text, client: { id: "desktop" } }`
   queues input for the running run: both see `steering_accepted` (seq 42, origin
   desktop).
4. The phone sleeps. On wake the browser reconnects with `Last-Event-ID: 42`;
   the host replays 43 to 47, the phone shows the finished reply and goes live.
5. Desktop: `run.start` while a run is active returns the busy error naming the
   running run, so it sends `run.follow_up` instead.

## Context fill and retained usage (updated 2026-10-20)

Durable per-request billing records now live in each chat's schema-8
`request_usage` table. `bubblepaw usage` and `bubblepaw usage export` read those
records, including archived chats, independently of the retained session
projection described below. See [usage tracking](usage-tracking.md) for the
fixed `bubblepaw.usage.v1` JSONL contract. Run totals and `RunFinished` retain
their existing aggregate contract.

`session.state`, `session.list`, `stats` session entries and the web session view
expose a `usage` projection. It contains `context`, `own`, `total` and
`descendants`. Descendant rows carry their own usage only; `total` adds the
parent and every nested descendant once, including hidden internal workers.
Inherited messages in a fork do not copy the source chat's usage.

`context.last_request_input_tokens` is the actual whole-prompt input count of
the most recent completed model response, including cached input. It is not the
cumulative input of the turn. It is `null` for older sessions without this
observation or when the latest response omitted usage. The accompanying
`last_request_observed_at` distinguishes an observed unknown count from absent
historical observations. Context fill describes that request, not a prediction
of the next request after new messages or compaction.
The HTTP adapters preserve the last physical request count across automatic
length-limit continuations while cumulative usage still sums every continuation.
If that final request omits input usage, the count remains `null` even when
earlier continuations reported tokens. The generic optional
`ModelResponse.last_request_usage` carries this distinction; custom providers
without it fall back to their returned response usage.
Its optional `output_tokens` records output from that same final physical
request. Compaction calibration uses the final physical input plus output,
never cumulative continuation billing. Missing physical counts clear an older
calibration anchor rather than falling back to aggregate usage.

`context.window_tokens` uses the explicit context window or an exact matching
entry in the local Codex model metadata cache; it remains `null` for unknown
capacity. `fill_ratio` is input/window, or `null` when either is unknown.
`compaction_trigger_tokens` uses the existing threshold calculation,
`min(window, compact_threshold_tokens) - output_reserve_tokens` (the cap is a
physical cap and the reserve is deducted once; children use
`child_compact_threshold_tokens` when it is smaller), with the configured cap
when the window is unknown. `auto_compaction_enabled` reports whether automatic
compaction is enabled. No model list refresh or provider request is performed.

Each totals object contains `tokens` (`input_tokens`, `output_tokens`,
`cache_read_tokens`, `cache_creation_tokens`, optional `reasoning_tokens`),
`recorded_entries`, `entries_without_usage`, `entries_without_pricing`,
`estimated_cost_usd` and `partial`. Cache input counts are subsets of input;
reasoning counts are subsets of output. Run totals and the runner's separately
recorded episode, chapter, decision and latest rolling-checkpoint usage are
included. The runner's context policies do not duplicate summary usage into run
totals. Failed and cancelled runs retain usage from their accepted responses
and known usage carried by classified provider errors. Malformed billable
responses remain rejected and do not update context calibration or last-request
observations. A failed HTTP continuation retains reported usage from preceding
physical responses plus the rejected response, when reported. A failure without
reported usage stays unknown. Cancellation can drop an in-flight provider future
before it returns final usage; those tokens cannot be reconstructed. Replaced
checkpoints, expired telemetry and unrecorded failures cannot be reconstructed,
so `partial` is always true and `basis` names retained
telemetry. A known zero-token run is not proof that its provider reported usage.

Costs use the runner's single dated price table, shared with the delegation
ledger and native usage report. Exact known model IDs select published uncached
input, output and cache rates, including the table's time/long-prompt rules.
Unknown models, unpublished used token classes and inconsistent cache counts
remain unpriced. A numeric cost in this retained projection sums only priced
entries and remains a partial estimate; it is never a subscription bill or an
all-time spending total. The native report separately identifies unpriced rows
and computes costs per request at read time.

The `UsageContext` adapter wraps both ordinary history and summarizing context
policies. Each completed response replaces one small `usage.last_request` host
state value and emits its normal `host_state_changed` event, tied to the active
run for journal retention. This adds one write transaction per response, no
schema, no history messages and no replayed effect. Reporting is read-only.
Archiving/archived roots have `usage: null` in lists; reporting does not restore
them. Unavailable descendants are listed in `unavailable_session_ids` and omitted
from the partial totals.

## Local process attachment

The private [host control endpoint](host-attach.md#control-protocol-version-2)
reuses the JSON-RPC envelopes and `event` notification records described here.
Protocol version 1 adds `attach.submit`, `attach.output` and a submission response
with `{exit_code,status}`. `run.cancel` on that connection cancels only its
submitted request; EOF has the same effect. This endpoint accepts `run` and
`delegate` submissions and is separate from the interactive stdio API above.
Version 2 keeps these records and replaces the submitted full environment with
filtered configuration references and request-scoped MCP credentials. Older
hosts and clients are rejected by the discovery protocol-version check.
