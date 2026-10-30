# Web host contract

Status: built; current as of 2026-10-29.

The web host embeds the library and owns a disposable in-memory serving SQLite
view. Core SQLite remains authoritative. The web feature is optional; default
CLI/library builds stay independent.

## Shared host API

The REST routes project results from the [shared host method table](host-api.md).
`POST /rpc` accepts the same JSON-RPC 2.0 request objects as stdio protocol 0.1,
with the same method names, parameters, results and error codes. All local calls
use `Profile::Full`; id-less notifications execute without a JSON-RPC reply.
The existing local boundary and request-size limits also protect `/rpc`.
SSE remains a transport concern and uses the shared subscribe/replay helper.

HTTP uses host-wide read scope so the chat list spans projects. Mutation workspace
checks remain in force. Malformed REST bodies return HTTP 400 before dispatch;
request size and content-type limits retain their own statuses.

## Startup

`bubblepaw web --port <port> --assets <dir>` serves on `127.0.0.1:<port>`; the
default port is 8788. `--port 0` binds an ephemeral port. `--assets` names the
built frontend directory (it must contain `index.html`). Once the listener is
bound, and before it serves, the host prints this line on stderr:

```
BubblePaw workbench: http://127.0.0.1:<port>
```

Callers that start the host, including the runner's tests, pass `--port 0` and read
the address from this line instead of reserving a port first. Match the prefix; other
stderr output is not part of this contract.

## Local boundary

Every request passes the same middleware (`local_boundary` in `web.rs`) before
its route runs:

- **Host.** The `Host` header must name `localhost` or a loopback IP address
  (`127.0.0.0/8` or `::1`) with the server's own port. Otherwise the response is
  403.
- **Origin.** If an `Origin` header is present, it must equal `http://<Host>`.
  This applies to every method, including GET. A mismatch is 403.
- **Content type.** POST, PUT and DELETE must send `Content-Type: application/json`
  (the attachment upload route takes `application/octet-stream`). Otherwise the
  response is 415.
- **Responses.** `/api/` responses carry `Cache-Control: no-store`. A non-2xx
  API response that is not already JSON is rewritten as `{ error }`.

The listener binds only to `127.0.0.1`. Responses carry no credentials: provider
payloads show key variable names and presence, never key values. Only built
frontend assets are served, from the explicit `--assets` directory.

## JSON endpoints

- GET `/api/config`: `{ workspace, model, provider, read_only, session_limit, input_limits }`.
  `read_only` is the tool access policy; it does not stop prompts from being sent.
  `session_limit` is 64. `input_limits` is described under Input files.
- GET `/api/sessions`: `SessionSummary[]`, at most 64 recently active sessions.
- POST `/api/sessions` with `{ title? }` (`name` is accepted as an alias): created `Session`.
  The default title is `New session`.
- GET `/api/sessions/:id`: `SessionView` (below).
- POST `/api/sessions/:id/rename` with `{ title }`: the renamed `Session`. Successful
  renames refresh session lists and publish a `reset`. A rename changes metadata,
  including `updated_at`, and calls no model. It is the `/rename <title>` host command.
- GET `/api/sessions/:id/messages?before=<message id>&limit=<1..200>`:
  `{ messages: Message[], has_more: boolean }`, oldest first. `before` is
  optional and exclusive; without it the newest messages are returned. The limit
  defaults to 100 and is capped at 200. Unknown cursors and invalid limits return 400;
  unknown sessions return 404. Pages read core history directly and are not cached in the serving view.
  `provider_state` is removed from each message (see `SessionView.messages`).
- POST `/api/sessions/:id/run` with `{ text, attachments?, options?, client? }`: admitted `Run`.
  Text that is a host command (`/rename`, `/rewind`, `/compact`, `/reload`) runs as that
  command instead, and returns its result. Attachments turn the input into a prompt.
- POST `/api/sessions/:id/steer` and `/followup` with `{ text, attachments?, client? }`:
  `QueuedInput`. Host commands are handled as on `run`.
- POST `/api/sessions/:id/cancel` with `{ client? }`: `{ requested: boolean }`.
- POST `/api/sessions/:id/resume` with `{ options?, client? }`: admitted `Run`.
- POST `/api/sessions/:id/fork` with `{ title? }`: created `Session`.
- POST `/api/sessions/:id/rewind` with `{ message_id }`: the rewind result, which
  includes the new `session_id`, `restored`, `deleted`, `recreated`, `conflicts` and
  `limitation`. It is the `/rewind <message-id>` host command. The source chat is
  unchanged. See [rewind](rewind.md).
- GET `/api/sessions/:id/processes`: `{ jobs: [...] }`, one entry per process owned by the
  session. Each entry has `handle` (`process:<uuid>`), `invocationId`, `command`,
  `status`, `exitCode`, `stdoutBytes`, `stderrBytes`, `outputBytes`, `spooledBytes`,
  `ageMs`, `backgrounded` and `foreground`. Without a process service the list is empty.
  Unknown sessions return 404.
- POST `/api/sessions/:id/processes/:handle/background`: no body. Marks the process
  as backgrounded and returns its output snapshot: `status` (`running`, `backgrounded`
  or an exit status), `handle`, `stdout`, `stderr`, `cursor: {stdout, stderr}`,
  `exitCode`, `capture` and an optional `note`. A malformed handle returns 400
  (`unsupported handle`); an unknown or expired process returns 404.
- POST `/api/sessions/:id/question/answer` with `{ question_id, text, client? }`:
  `{ question, live }`, where `question` is the answered question object. `live` is
  true when a waiting tool call was still attached and received the answer; false
  when the answer is saved for a generated continuation. Only the root session
  answers. A child session's question can be answered by its direct parent through
  `agent_send` or by a host user through the explicit child route below;
  this root route rejects child sessions before it changes any state. The
  rejection is a 409 (see Errors). The question lifecycle is in [session API](session-api.md#todo-and-questions-built-2026-10-15).
- POST `/api/sessions/{parent}/children/{child}/question/answer` with
  `{ question_id, text }`: `{ answered: true }`. Both sessions must belong to this
  host's workspace and `child` must be a direct child of `parent`. The exact
  question ID is required. Unknown/inactive children, wrong direct parents and
  unknown question IDs return 404; an answered/cancelled question returns 409;
  blank text or text over 16 KiB returns 400. The child retains its original
  harness/profile. A live waiter receives the answer; an interrupted waiter gets
  a safe generated continuation. The child's question records `answered_by:
  "user"`. Durable `host_state_changed` events update its `question` and the
  parent's separate `child_question_answer` key (answered object plus
  `child_session_id`), a bounded latest-answer projection. The parent's own
  pending question remains unchanged. The two state changes commit separately.
- GET `/api/sessions/:id/approvals`: pending `ApprovalRecord[]`.
- POST `/api/approvals/:id` with `{ decision: "approve"|"approve_session"|"deny", reason?, client? }`: decided `ApprovalRecord` ([approvals](approvals.md)).
- GET `/api/events`: SSE stream, described under Events.

`client` is the optional `{ id, name? }` of the sending device (`id` 1 to 256 bytes,
`name` up to 256). The event the command causes carries it as `origin`
([session API](session-api.md#origin)).

`SessionSummary`: `{ id, title, created_at, updated_at, status, message_count,
last_message_preview }`. `status` is the status of the session's latest run, or
`idle` when it has none. Run statuses that can appear are `running`,
`waiting_for_quota`, `completed`, `cancelled`, `failed` and `interrupted`:

- `waiting_for_quota`: the run is waiting for a provider account to reset its
  quota ([quota](quota.md)). It returns to `running` on resume, or ends as
  `cancelled` or `failed`.
- `interrupted`: the process was lost while the run was active, or while it was
  waiting for quota.
- `waiting_for_answer` replaces the run status while the session has a pending
  question (`question` is non-null). The session view always shows it. The sidebar
  shows it only for active (not archived) sessions.

`SessionView`: `{ session: SessionSummary, messages: Message[], runs: Run[],
pending_inputs: QueuedInput[], active_run_id: string|null, live_paragraphs: string[],
event_sequence: number|null, seq: number, live_seen: number, limits: ViewLimits,
outputs: OutputRecord[], repl: object|null, approvals: ApprovalRecord[],
usage: SessionUsage, todo: TodoItem[], question: object|null }`.

- `messages` are core `Message` values. The view clears `provider_state` on each
  message, so the field is omitted. Tool calls and results stay available for
  expandable action details. The page `/messages` endpoint does the same.
- `runs` are run metadata without `Run.text`. Canonical messages hold completed answers.
- `pending_inputs` lists only pending items.
- `active_run_id` is the id of the run whose status is `running`. A run in
  `waiting_for_quota` is not reported there; the session status still shows it.
- `seq` (and its older name `event_sequence`) is the last durable event the view
  reflects. `live_seen` is described under Events.
- `usage` is the session usage projection (`context`, `own`, `total`,
  `descendants`) described in [session API](session-api.md#context-fill-and-retained-usage-updated-2026-10-20).
  In session lists, archiving and archived roots show `usage: null` instead.
- `todo` is the chat's latest todo list (`[]` by default). `question` is the pending
  question object, or `null`.
- `limits` is `ViewLimits`: `message_limit` (200), `run_limit` (64),
  `pending_input_limit` (128), the truncation flags `messages_truncated`,
  `runs_truncated`, `pending_inputs_truncated` and `live_paragraphs_truncated`.
  Earlier messages stay in the archive and are read through
  `GET /api/sessions/:id/messages`.

Errors: a non-2xx response is JSON `{ error: string }`. The status comes from the
error text (`ApiError` in `web/errors.rs`), with typed child-answer errors mapped
by the child-answer handler:

- 400: malformed JSON, text or attachments over the input limits, an unknown
  message cursor, a bad title, an unsupported process handle, a read-only refusal,
  `/reload` on a host without live config, and blank or oversized child-question answers.
- 422: a JSON body that parses but has missing or mistyped fields (the JSON
  extractor's own rejection).
- 403: Host or Origin rejected by the local boundary.
- 404: unknown session, unknown or expired process or artifact, and unknown
  provider; child-answer requests also use 404 for inactive children, wrong direct
  parents and unknown question IDs.
- 409: active-run conflicts, a session bound to another workspace, a question answer sent to a child session, and provider
  saves masked by a later config layer, and child questions already answered or cancelled.
- 415: a POST, PUT or DELETE without the required content type.
- 500: any error the mapping does not classify. Clients should treat unexpected
  500s as failures to show, not as state.

Preserve drafts on client errors. POST requires JSON (see Local boundary).

## Provider setup

Provider setup endpoints used by the [web workbench](web-workbench.md). No request
or response carries a key value; mutations use the same local boundary as other POSTs.

- GET `/api/providers`: `{ target: {path, layer: "custom"|"user", exists}, kinds, main,
  launched_main, active_main, reload_needed, restart_needed, allowed_key_envs: string[], providers: [{ name, kind, base_url, key_env,
  key_present, auth_file, reasoning, models: [{id, image_input, reasoning_replay}], source,
  inherited_fields, editable, deletable }] }`.
  - `image_input` defaults to true for the `codex`, `responses`, `chat-completions`
    and `anthropic-messages` kinds, and to false for other kinds. A model entry can
    set it explicitly.
  - `reasoning_replay` is `none`, `tool_loop` or `all`. It defaults to `tool_loop` for
    `chat-completions` providers and to `none` for the others.
- PUT `/api/providers/:name` with `{ kind, base_url?, key_env?, auth_file?, reasoning?,
  models }`: writes and immediately applies the provider table; returns the GET payload.
  400 for invalid input or clearing an inherited field (`base_url`, `key_env` or
  `auth_file` inherited from defaults), 409 when a later layer masks the save.
- DELETE `/api/providers/:name`: only for providers in the target file (else 404); 409
  when it is the main model's provider. A successful deletion applies immediately.
- PUT `/api/roles/main` with `{ model: "provider/model" }`: writes and immediately applies `[model] main`.
- POST `/api/config/reload`: applies [live config](live-config.md) from the original
  layers. The response depends on the host (see Reload below).
- POST `/api/providers/:name/test` with optional `{ draft }`: `{ config: {ok, error?},
  reachable, auth: "ok"|"failed"|"unchecked"|"not_applicable", auth_detail,
  models_listed, configured_models: [{id, listed}], detail, elapsed_ms }`. Never calls a
  generating endpoint. Without `draft`, an unknown provider returns 404. A draft that
  fails validation returns 200 with `config: {ok: false, error}`.

### Reload

`POST /api/config/reload` has two responses, depending on how the host was started.

- **Host with live configuration** (every `bubblepaw web` host): the reload runs
  and the response is the GET providers payload plus `applied` and
  `diff: {applied, changed: string[], errors: string[], restart_needed, active_main}`.
  When `applied` is false the response is 400 with the same payload and an `error`
  summary. Active settings are unchanged in that case.
- **Host without a host config** (library and test setups): the response is the bare
  GET providers payload, with no `applied` or `diff`. Invalid config returns 400
  with the error text and leaves active settings unchanged. With no live provider
  configuration at all, it returns 409.

Stdio `config.reload {server?}` returns the diff directly. `/reload [server]` sent as
the whole input of a run, steer or follow-up returns `{reload: diff}` without creating
a model run; the diff's `applied` may be false and the reply is still 200. A host
without live config answers `/reload` with 400. All host commands share one executor
with stdio and CLI; see [session API](session-api.md#host-commands-and-session-control-built-2026-10-22).

PUT provider and POST Test (both draft and saved settings) return 400 for a
non-loopback base URL that does not use HTTPS:
`base_url must use https unless it points at this machine (localhost/127.0.0.1/::1)`.
Loopback includes `localhost`, `*.localhost`, `127.0.0.0/8` and `[::1]`.
Base URLs with credentials, queries or fragments remain invalid; error details
never echo those URLs.

They also return 400 when a valid identifier in `key_env` does not end in
`_API_KEY`, `_KEY` or `_TOKEN` and is absent from the effective `[web] provider_key_envs`:
`key_env NAME is not an allowed key variable; names must end in _API_KEY, _KEY or _TOKEN, or be listed in [web] provider_key_envs`.
The allowlist defaults to `[]` and is read afresh on each request. `allowed_key_envs`
exposes only its variable names, never values.

Saved setup applies immediately. `active_main` is the qualified active model;
`launched_main` remains the startup value and `main` is the effective disk setting.
`reload_needed` marks unapplied live disk changes; the turn-start stat check or
explicit reload applies them. `restart_needed` flags storage, context, web-cache
and extension changes and stays false for live-only changes. Startup and reload enforce the same
URL and key-variable rules. In-flight requests retain old settings and the next
request/round uses the replacement; a failed apply retains active settings. See
[configuration](config.md#live-configuration-2026-10-14) for reload scope.

## Input files

Config includes `input_limits: {prompt_chars, max_file_bytes, max_total_bytes,
max_files, inline_tokens}`. Defaults are 32,000 Unicode scalar characters,
16 MiB/file, 32 MiB/message, 16 files/message and 10,000 O200k-estimated tokens
for the complete attachment envelope. Plain prompts are never silently cut.

POST `/api/sessions/:id/attachments?name=...` accepts raw octet-stream bytes
within the file limit. Its response is `{id, name, size, path, url, text}`:
`path` is workspace-relative, `url` is a session-owned download endpoint, and
`text` is true when the file is UTF-8 text. GET
`/api/sessions/:id/attachments/:attachment` returns the original bytes as an
octet-stream download with `nosniff`. Unknown or foreign IDs fail. Upload body
limits are enforced while reading, independently of the bounded JSON body limit.

Run, steer and followup JSON accept optional `attachments: string[]` IDs, with
`text` allowed to be empty when files are present. Before admission the host
checks ownership, duplicate IDs, count and aggregate byte limits. It appends a
`BubblePawAttachments` JSON envelope to the canonical user message. Whole UTF-8
text files are inlined in the envelope only when it stays within its token budget;
otherwise the reference carries `omitted: true`. Binary files remain references.
The original prompt text is preserved.

The composer converts an oversized paste to a removable `paste.txt`, validates
file selections as a batch, and uploads only on submission. Failed sends keep
the draft and uploaded IDs for retry. Accepted messages display download chips
and optional collapsed text excerpts. Staged local file bytes are held in memory,
not browser storage. Files live outside SQLite under `.run/attachments/` and
survive host restarts; abandoned uploads currently need manual cleanup.

## Events

GET `/api/events` is an SSE stream. Each core event is a default `message` event
whose data is the `HarnessEvent` JSON; the kinds are listed in
[session API](session-api.md#event-kinds). The SSE `id` is the event's sequence:
a durable event's own log sequence, or, for a live event (`durable: false`, such as
`paragraph` or `text_delta`), the last durable sequence before it. A named `reset`
event tells the client to refetch the session list and the selected session.

- A new subscription without `Last-Event-ID` starts with `reset`.
- With `Last-Event-ID: N` (the browser sends it on reconnect), the host replays the
  logged durable events after N, then goes live without repeating them. Paragraphs
  and text deltas missed while disconnected are not replayed. Replay drops events of
  hidden sessions, and so does the live stream. The host sends `reset` instead when the log cannot answer: N is
  older than retention pruned, N is beyond the last sequence (another archive or a
  fresh host), or more than 10,000 events were missed.
- A `reset` is also sent on the live stream when the broadcast lags, or when the host
  resets its own state.

Durable state remains authoritative. The host keeps bounded transient paragraph
state for recovery during a running request. Paragraph events do not write to the
archive or serving SQLite; durable events go to the archive's event log
([session API](session-api.md)), which does not change `revision` or
`transaction_count`. `seq` (and its older name `event_sequence`) is the last
durable event the view reflects; `live_seen` is how many of this session's live
events carrying `seq` its `live_paragraphs` already include. The client applies
durable events with a sequence above `seq`, skips the first `live_seen` live
events carrying `seq`, and applies later ones.

The serving cache holds at most four session views and 16 MiB serialized data;
oversized views are read but not cached. Live paragraphs are capped at 256 pieces
and 512 KiB per active session. At most 64 sessions hold live state; when another
run starts past that point the host clears all live state and sends `reset`.
These limits bound presentation state independently of canonical history.

## CodeMode additions

- GET `/api/sessions/:id/environments`: `{ environments: [...] }` with volatile
  generation/status, wait counts, state previews, handles and execution identity.
- POST to the same route: `{action, environment}` for create, inspect, background,
  wake, cancel, reset or close. Reset/close of busy environments requires cancel.

These environment controls and session fork remain host APIs. The basic web
frontend exposes neither management UI; it keeps chat and ordinary run controls.

- GET `/api/sessions/:id/artifacts/:artifact`: immutable session-owned capture;
  unknown IDs or another session return 404. Raster images render inline; other
  files download, with bounded names and `nosniff` headers.

Output SSE events carry durable `OutputRecord` values. Session snapshots include
up to 128 recent output records. Original run/execution ownership survives steering
and background completion. Environment controls cause a reset/refetch; no heap or
process-output stream is persisted. See [CodeMode](codemode.md) for API limits.
