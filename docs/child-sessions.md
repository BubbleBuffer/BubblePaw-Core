# Child sessions

Status: recursive budgets and opt-in worktree isolation built 2026-10-15; children built 2026-10-13; controls built 2026-10-14; hidden background workers built 2026-10-14; idle completion wake built 2026-10-14. Verification uses scripted and
in-process providers, plus the dedicated signed OpenCode research-child acceptance
recorded in [provider qualification](provider-qualification.md). The background-worker change itself makes no live model acceptance claim.

The runner provides long-lived child sessions. A parent starts a child, sends
input or steers it, checks status or waits for a result, and cancels it. Ordinary
child completion also queues a reminder and wakes an idle parent automatically
in long-lived hosts. Internal workers stay silent toward their callers, while
their own children can wake them.

## Design

### Placement and storage

`crates/bubblepaw-runner/src/agents.rs`, `profiles.rs` and `agent_handles.rs`
own orchestration. Core provides generic per-run structured output and tool-call
budgets, effective `ApprovalPolicy` on `ToolContext`, host notice delivery and
projections from durable child run boundaries; orchestration stays in the runner.
Questions use the generic bounded durable
host-state table; orchestration metadata otherwise retains its existing storage.
The generic transient `ModelRequestStarted` event resets the host's partial-answer
buffer for each provider request, including a schema correction; it adds no durable row.

Each child has its own session, created with `Store::create_session_with_parent`.
Its catalog parent/root links place it in the parent's `chats/<root-id>.db`.
There is no consolidation: the parent receives tool results, and the full child
transcript stays readable through normal session APIs. Only explicit task/context
is transferred; parent history and JS state are not copied.

Each child independently freezes its host workspace's [memory files](memory-files.md)
at its first turn. It does not copy the parent's frozen instructions: a child
started after an instruction-file edit sees the new content while its parent
keeps its original prefix and receives a durable change note.

### Tool surface and lifecycle

Inside `js`:

- `tools.agent({task, context?, profile?, worktree?, output_schema?, max_tool_calls?, timeout?})` creates and starts a child,
  returning `{handle:"agent:<uuid>", sessionId, status:"running"}`.
- `tools.agent_send({handle|sessionId, text, question_id?, output_schema?, max_tool_calls?, timeout?})` steers a running child through
  `Harness::steer`, or starts a new turn in an idle child through `Harness::start`.
  When the child is `waiting_for_answer`, `text` answers its pending question
  and resumes its current tool invocation. Optional `question_id` correlates an
  answer with that question and rejects stale answers. Run controls cannot change in an answer.
  Every ended child remains continuable, including cancelled, timed-out and failed runs.
- `tools.agent_status({handle?|sessionId?})` returns one child's status and its
  available result. With no address it lists the calling parent's descendant tree,
  including `state`, `handle`, `sessionId`, `parentSessionId` and `unread`.
- `wait({handle|sessionId, timeoutMs?})` returns the turn's result, including
  text, available usage and any error. `timeout` leaves the child running.
- `tools.cancel({handle|sessionId})` cancels the child and its subtree and awaits
  cleanup.

States are `running`, `waiting_for_answer`, `idle`, `cancelled`, `timed_out` and `failed`. A successful turn leaves
the child `idle` and reusable; `wait` retains `status:"completed"` for that turn.
Commands and JS environments are released after each turn and recreated on the
next. Replies fit within 60 KiB, with explicit truncation; full text remains in
canonical child history. Task and serialized context together fit within 64 KiB.

A finished result is unread until `wait` or `agent_status` for that specific child
returns it. Listing all children does not consume results. A parent can inspect
the list or receive a completion notice to find work that finished while it was idle.

### Durable events on the direct parent stream

Clients following just the parent session receive
`child_run_state_changed {child_session_id, state}` in an ordinary durable event
whose `session_id` is that direct parent and `run_id` identifies the child run.
The states come from existing durable run boundaries: `running` on admission
and the actual stored terminal status on completion. This does not persist
registry-only `idle`, cleanup progress or quota-wait transitions. A child timeout
can therefore have a runner result of `timed_out` and a stored run status of
`cancelled`. The run projection shares the existing transaction and journal
batch; ordinary parent turns add no transaction for it.

Startup interruption recovery is also observable as `state: "interrupted"`.
Single-file stores commit that recovery and its parent projection atomically
with no added transaction. Normal chat-v1 directory stores finish canonical
recovery first, then publish the parent event after catalog/sequence
initialization, adding one transaction per recovered visible child run. There
is no durable outbox. A failed directory reopen or crash before the queued
projection flush can require reset/resnapshot; the next startup's sequence
reservation invalidates old cursors. Parent-event replay through legacy
intermediate storage conversion is not guaranteed. This adds no persistence for
registry idle or cleanup progress.

Questions project pending/cancelled changes to the parent's `child_question` key
and answers to its existing `child_question_answer` key. These are bounded latest
values with the child's identity, question content and state; they leave the
parent's own pending question untouched. Same-database projections share the
question transaction. These bounded parent values are event hints, not a pending
inventory; the child's question is authoritative. Run projections skip
hidden/internal children; question projections retain the existing parent-question
behavior, including hidden worker parents. Grandchildren project only to their
own direct parents.

Stdio advertises `child_events` in `initialize.features`. Its existing event
notifications, parent-scoped `events.after` and the existing web SSE route carry
the same envelopes. Clients derive waiting from a pending question, then
resnapshot or follow subsequent events. See [session API](session-api.md#child-events-on-the-direct-parent-stream)
for exact fields, replay and transaction limits.

Handles are process-local. An address using `sessionId` is accepted only when the
store's catalog parent is the calling session. After restart, addressing a saved
child reattaches it with a fresh handle; old handles are not replayed. Listings
include canonical children whose handles are not retained, with `handle:null`
and their durable `sessionId`, without allocating a handle for each listed child.
Finished results remain readable by session id when retained slots are busy.

The original profile/model/round binding, cancellation intent, cleanup failures,
and per-turn read receipts use existing immutable, session-scoped attachment
metadata. They do not enter model history. Read receipts preserve unread state
across handle eviction and restart. Rediscovery preserves the last turn's status;
an explicit follow-up starts a new turn with the saved history.

Profiles default to `read-only`; `workspace` permits commands and writes when
the host permits them. Optional
profile model and round limits continue to apply. Nested children cannot broaden
the parent's profile capabilities. Workspaces are shared by default;
`worktree: true` opts into a separate git checkout. See [child worktrees](child-worktrees.md).
A separate session does not imply process sandboxing.

### Structured output, tool budgets and timeout

All profiles accept these controls through `AgentTools::start` and the model's
`tools.agent` API. `AgentTools::send` and `tools.agent_send` accept them for a new
turn. Generic library callers also set `RunOptions.output_schema` and
`RunOptions.max_tool_calls` directly.

`output_schema` has the same shape as a direct provider request:
`{name: "answer", schema: {...}}`. Object schemas explicitly set
`additionalProperties: false`. The run passes the schema to the provider's native
structured-output API and validates the complete final JSON. An invalid answer
gets one tools-disabled correction request containing the validation error.
A second invalid answer returns `status:"failed"` with a clear validation error.
Successful schema replies contain the complete validated value in `json`, alongside
`text`. A value that cannot fit the existing child reply bound returns a clear
error rather than a truncated JSON success. Full answers remain in child history.

`max_tool_calls` is one nonnegative integer for the turn. Omission is unlimited
unless the calling parent turn has a remaining bound;
zero requests an immediate tools-disabled answer. The task states
"You have N tool calls" up front. Every model-facing tool result ends with
"N tool calls left". The JS wrapper and each native call inside it count
individually toward the same budget. There are no per-tool limits. Concurrent
calls reserve slots before approval or effects; calls beyond the remainder get
an explicit budget-exhausted result. Once the remainder is zero, the next model
request still carries the tool declarations, with a reminder that tool calls are
unavailable, so the child answers from its existing evidence, with the output
schema when set. A call it still makes is refused, and only then are the tools
switched off for the rest of the run.

The tool budget is a `tool_budget` reminder: a generated user message, sent at run
start and once more as a warning near exhaustion. The system prompt stays stable across rounds.
The first request with no calls left keeps the tool declarations and queues a
`tools_off` reminder ("Tool calls are unavailable for the rest of this run. Answer in
text using the information you already have."), so it is an append with no
tool-choice change, and signed thinking keeps its original conversation prefix. If the
model still calls a tool, that call is refused without running, and the next request
moves the declarations to inactive `disabled_tools` and sends the provider's tool
choice off (`none`). The schema correction round follows the same rule.

`timeout` is an optional nonnegative integer in milliseconds, measured from the
launch of that turn. It covers provider work and tools. On expiry, the host
cancels the current run through the existing cancellation path, awaits cleanup,
and returns `status:"timed_out"` with the last partial answer when available.
It leaves the saved child and its history available. A wait's `timeoutMs` remains
a wait-only limit: `status:"timeout"` leaves the child running.
Unfinished streamed text is returned to the live caller and remains transient;
after restart, saved timeout results retain only available complete canonical text.

Every ended child accepts a normal follow-up with its full history, whether it
finished, exhausted a budget, timed out, was cancelled, or failed schema validation.
Follow-up `max_tool_calls` and `timeout` apply only when explicitly supplied;
otherwise the new turn has unlimited calls and no deadline, subject to the
calling parent turn's remaining budget and deadline. A supplied new budget
is stated in the follow-up the same way as the original task. The output schema
persists across follow-ups; a replacement changes it and `output_schema:null`
clears it. Limits on an already running turn cannot be changed by steering.

### Stale-child reminders

`agents.stale_reminder_secs` defaults to 7,200 seconds. Zero disables reminders.
Before a parent model request, the existing `Tool::take_reminders` hook checks
running children. A child whose parent has not sent a message, polled status,
waited, or read its result for that interval yields a reminder identifying it,
its running age, and the request to check, message or cancel it. A reminder repeats
at most once per interval per child. Interaction resets the interval; completed
children produce no reminders. Notices do not wake an idle parent or impose
default child timeouts.

### Effects and recovery

Spawn and an idle-child send are non-read-only tool calls returning `pending`.
Each started turn's parent effect settles through `ToolDispatcher::complete_effect`
only after the canonical turn ends and cleanup succeeds. Cleanup uncertainty
leaves the effect unresolved. Admission rejection returns an ordinary error
result, so it completes the dispatch row without claiming an external outcome.

A steer into a running turn settles immediately and is accepted child input,
without another long-lived effect. Restart recovery marks interrupted effects
`unknown`, including the child's own effects in the same chat file. The next send
resumes marked history. Unknown external effects are never automatically replayed.

### Approvals

A child's approval policy is clamped against the parent's effective call policy.
Live host profiles inherit a resolver bound to that originating parent call:
ordinary inherited policy follows config edits for the next child tool call,
while an explicit parent run override remains pinned. Static embedded profiles
continue to use an explicit `RunOptions.approval` snapshot. Profile restrictions
clamp either path.

The rank is `Deny < Ask < Auto < Allow`. The stricter parent/profile mode wins for
`confirm` and each per-tool mode; `timeout_secs` is inherited from the parent.
Child approval requests appear on the shared store event log under the child's
session id, so the parent's host subscription sees and can decide them.
Unattended one-shot `run`/`resume` continues to deny requests.

### Limits and cancellation

The layered [`[agents]` configuration](config.md#child-session-limits) leaves
`max_depth`, `max_active_per_parent`, `max_active_total` and `max_retained` unset
by default. Unset means no limit: workspace profiles receive agent tools and can
start children at any depth. Research children may delegate to the same research
profile; workspace profiles retain their own capability ceiling. Optional caps
are available against runaway recursion.

Root depth is zero, counted along the catalog parent chain. An explicit depth
cap rejects deeper starts; zero disables spawning. Workspace profiles retain
weak child controls at every depth, so raising a live cap (including an initial
zero) allows deeper children immediately. Active caps count running turns and
apply again to idle sends. Host-wide caps are shared across depths. Retention
bounds only in-memory handles: evicted children remain in the catalog, appear in status listings and receive a fresh
handle when addressed by `sessionId`. Cancellation includes idle descendants
whose handles have been evicted.

A hosted `AgentTools` follows the published `agents` section (`AgentTools::follow`), and `apply_limits` sets it standalone; either way the shared host settings apply to future child
admission, wakes, reminders and retention. It does not replace a running turn's
explicit tool budget or wall-clock timeout. Child harnesses obtain a fresh root
tool registry at each of their own turn starts (and after explicit reload), then
reapply their profile restrictions, weak child controls and isolated JS wrapper.
Research keeps its independent source tools and read-only workspace/reload
allowlist. Live web enablement makes research available even when web was disabled
at host startup; disabling it hides new research admission while preserving
existing child inspection and continuation. Live hosts also keep the workspace
profile prebuilt, hiding new admission and its schema while `approvals.read_only`
is true and exposing it again when false; existing child recovery is unchanged.

Cancelling a parent cancels its subtree deepest first and awaits cleanup.
Children otherwise run alongside the parent's turn and can continue after that
turn ends. Parent steering backgrounds an explicit wait. Visible completion
notices use the normal request hook or start an idle parent turn as described below.

### Questions (user answers built 2026-10-27; original path 2026-10-15)

`tools.question({text, options?})` suspends a CodeMode host call. Root questions
are exposed to clients as durable pending question state; unattended root hosts
return `no_user_available` immediately. Options are suggested strings, and free
text answers are accepted. At most one question is pending in a session. Text
and answers are bounded to 16 KiB each; there are at most 16 options of 1 KiB.

A child's question goes to its direct parent as a host notice, with the child's
session id and question. It wakes an idle parent through the ordinary wake
lifecycle, including when optional completion wakes (`agents.auto_wake`) are
disabled. Explicit cancellation suppression still applies. The parent responds
with `agent_send`, and child status includes `waiting_for_answer` plus the pending
question. Waiting suspends CodeMode's ordinary host-call idle deadline. Cancelling
the session or its subtree cancels the pending question and releases the waiter.
An explicit parent `wait` returns `waiting_for_answer` when a question arrives,
so its JS cell finishes and the parent can answer from the same environment on
the next model round. The child's task stays suspended until answered.

A host client can answer on the direct parent's behalf with
`QuestionTools::answer_child_as_user(parent, child, question_id, text)`, the SDK's
`Host::answer_child_question`, stdio `question.answer_child`, or
`POST /api/sessions/{parent}/children/{child}/question/answer`. This is a host-only
operation, never a model tool. It first checks the child's catalog parent, then
the question ID and pending status, then the answer's nonempty/16 KiB bound.
A wrong parent, missing child or unknown question is not found; an already
answered or cancelled question conflicts; invalid answer text is rejected.
Resolving the question checks and changes its JSON host-state row atomically.
A racing parent answer and user answer cannot both win: the loser gets an
already-answered conflict.

The child receives a user-authored answer through the same neutral generated
answer notice, durable question marker and live-waiter or recovered-continuation
path as a parent's answer. Its answer notice does not identify the author.
The stored answered question and its durable state-change event carry
`answered_by: "parent" | "user"`; old answered rows without that field read as
`"parent"`. The parent's `child_question_answer` host-state row projects the
latest answered child question, including `child_session_id` and `answered_by`,
and emits its own durable state-change event. Pending and cancelled questions
use the parent's separate `child_question` key. The child's `question` row remains
the authority; projections share its transaction when both sessions use the same
database and never replace a question asked by the parent itself. A user answer also sends the parent a required
`question_answered_by_user` notice identifying the child, question ID and a
bounded preview of the answer. It uses the waiting notice's key, replacing an
undelivered `question_waiting` notice for that question. Optional completion-wake
settings do not disable it; explicit cancellation suppression still applies.
A later `agent_send` answer correlated with that question ID reports that the
user already answered and never delivers a second answer. Direct-parent
ownership and ordinary child continuation rules are unchanged.

The question and answer live in the authoritative chat's bounded host state,
with generic durable state-change events. Before delivering a live answer to its
waiting tool, the host durably accepts it as generated steering input, with a
neutral answer notice and question id marker. Sequential questions in one JS cell
are supported: an earlier answer stays queued when the next question replaces
the host-state row. Ordinary input consumption places those notices into canonical
history once, making disposable run metadata unnecessary after retention.
Answer-state update and accepted input each cost one transaction; canonical input
consumption batches nearby answers at the existing turn boundary. A child's
question projection shares that transaction in the same database; a parent in
another database requires at most one additional transaction per change. Unchanged
notice checks add no transactions. Startup republishes unanswered child
questions. A lost JS environment is never reconstructed: after restart, an answer
is accepted as a fresh generated continuation input containing a deterministic
question id marker. Retrying that answer checks queued input and canonical history
to avoid admitting the continuation twice. The answered state remains available
if admission fails; the next ordinary model request also recovers that saved
answer once through the durable host-notice path. Restart itself starts no model
turn for a saved answer. Original messages and unknown external effects stay intact;
the interrupted cell is never replayed. Child session-id addressing retains the
existing direct-parent ownership check.
The merged child lifecycle also registers hidden worker parents for their own
ordinary children's questions. A question wakes that idle hidden parent through
the same binding, and the parent answers with `agent_send`; the worker stays
silent toward its visible caller. The earlier control-only qualification's
hidden-parent limitation no longer applies to this merged lifecycle.

Scripted acceptance is in `tests/questions.rs` and `question::tests`: actual
CodeMode child questions wake idle parents with optional wakes enabled and
disabled, `agent_send` resumes the original turn, root/client and direct-parent
answers work, cancellation releases waits, and a frozen restart fixture preserves
an unknown effect while accepting a durable answer once. No live-model acceptance
or real process-crash proof is claimed.
`tests/question_routing.rs` additionally qualifies grandchild questions and
ordinary children of hidden workers through actual CodeMode calls. Each fixture
first leaves the direct parent idle, then releases its child's question, verifies
that only the immediate parent wakes, rejects an ancestor's attempted answer,
and resumes the original child run via the parent's `agent_send`. Optional
completion wakes are disabled to isolate required question delivery.

### Completion notices and idle wake

Every terminal visible child turn queues a completion notice, including success,
budget-exhausted final answer, timeout, failure and cancellation. The notice
identifies the child, its terminal state and available result. Internal workers
produce no outward completion notice. Their ordinary hidden
children notify and wake that direct parent only, using the same child lifecycle.
Timeout notices use canonical text; streamed partial text remains available in
the live child result without being copied into the durable wake input.

Long-lived web and stdio hosts wake an idle parent by admitting one durable
generated reminder message. Nearby child and background exec completions share
one turn after `agents.wake_debounce_ms` (default 1,500 ms). `agents.auto_wake=false`
disables scheduling while preserving ordinary next-request reminder delivery.
Generic core notice handling keeps accepted generated host input in the model
request when the host replaces its serving history; original history remains intact.
Busy parents consume reminders through the typed reminder hook; a consumed
reminder cannot also schedule an extra wake. Explicit result reads remove that
child's pending completion notice.

Stopping/cancelling a parent suppresses automatic wakes until accepted user text.
Archived and closed sessions are ineligible. Hidden roots cannot use the root
fallback; registered hidden worker parents receive their own children's wakes
without notifying their visible caller. Wake chains are uncapped; child parents
use the same continuation and cleanup path. Wake scheduling
is process-local: restarting never wakes a parent for an old completion and never
replays an external effect with unknown outcome. Completion information remaining
after restart is supplied on the next explicit parent turn.

Library hosts opt into the lifecycle through the runner's `wake::WakeCoordinator`:
share it between agent and process tools, retain a root harness registration and
a wake driver, and notify it of explicit stop and accepted user input. Attach the
store to process tools for durable known-completion recovery. Dropping the driver
stops scheduling. Root wakes use the harness's normal new-turn defaults; child
wakes retain their saved profile/model/approval binding and output schema, with a
fresh unlimited tool budget and no timeout. One-shot commands end at their ordinary
turn boundary. The CLI has no separate interactive terminal REPL host; CodeMode
environments run inside these hosts.

## Background workers

Internal host jobs use the same child launch, Harness execution, observation and
cleanup path as visible children. `AgentTools::start_worker` accepts a host-built
`AgentProfile`; the model-facing `tools.agent` API cannot request internal status.
Workers accept one job and reject follow-up turns; visible children remain reusable.
Worker sessions carry a generic hidden flag in the existing stored Session JSON.
It also propagates to their descendants. Host session lists, web projections,
child status listings and stale-child notices exclude hidden sessions by default.
Canonical lookup by explicit session id remains available for inspection.

Workers use the ordinary child `output_schema`, `max_tool_calls` and timeout
controls, approval clamping, reasoning replay and effect rules. There is no
second model/tool loop or default worker concurrency cap. Explicit host child caps
still apply. The shared registry tracks cancellation and cleanup.

Successful completion alone does not delete a worker. The caller receives or
persists its result, then calls `acknowledge_worker` to delete it. This avoids losing results between completion and
delivery. `acknowledge_worker_turn` binds acknowledgement to the launch receipt's
run id; probe results use this check and reject mismatched receipts.
`agents.worker_keep_all=true` preserves successful chats too. Failure,
cancellation and timeout keep their canonical history until
`agents.worker_retention_days` expires (default seven days). Cleanup skips live
sessions and any subtree whose effect outcome remains unresolved. Unknown
external effects are never replayed. Explicit session inspection uses existing
storage APIs; deletion is the ordinary bounded subtree deletion.

Replay probes inherit the loaded agent policy too. Library callers can supply it
through `ReplayOptions.agents` or `probes::answer_with_policy`; the simpler probe
entry point uses defaults.

Retention cleanup runs at runner startup, worker launch and observed child turn
completion, or through the host's explicit `cleanup_workers` API. It does not
delete successful workers awaiting acknowledgement. Hidden child completion releases
provider mirrors and parsed history caches before publishing the result, including
ordinary descendants of workers; cleanup failure retains its failure metadata.
Ordinary hidden descendants can rebuild from canonical history for a follow-up.
Visible children keep their
ordinary continuation behavior. Worker session ids in probe results
allow inspection without adding them to default user lists.

Provider mirror disposal has a one-second host cleanup bound, separate from the
optional model/tool turn timeout. Disposal error or timeout retains a failed
worker. Successful parent acknowledgement also checks descendant outcomes and
cancellation markers, so it cannot erase an unsuccessful child or unknown effect.

See [verification](verification.md).

## Research profile (2026-10-13)

With web access enabled, `research` is an ordinary visible child type. Start it
with `tools.agent({profile:"research", task})`, wait or inspect its status, and
ask follow-ups through `tools.agent_send` using the same handle or session id.
The child stays openable through the normal session APIs and its transcript
remains canonical. There is no synchronous `research` tool or separate model loop.

Its capabilities are `web_search`, `web_fetch`, workspace file reads and the
ordinary JS wrapper, plus same-profile child controls. It cannot write files or
run commands. It
chooses its own searches and fetches within the host's ordinary round limit and
any explicitly configured miss budgets. It inherits the active parent model;
custom embedders can set `AgentProfile.model` as for any profile.

The parent also receives web search/fetch for quick lookups. Both use the same
disposable SQLite cache. Source IDs (`s1`, `s2`, ...) persist in the root chat and
are shared by parent and descendants. Children cite these IDs and passage
references (`s3#p12`); the parent resolves them through the web tool source mapping.
See [web tools](web.md) for passages, retrieval and cache behavior.

## Settled 2026-10-13

Child orchestration stays in the runner. SQLite history stays authoritative
without a schema change. Core hooks provide generic run controls, notices,
request boundaries and effective approval in `ToolContext`; full results remain
available through `wait` or `agent_status`, with completion notices and idle wake
added in the 2026-10-14 decision. Every ended visible child remains continuable, and
volatile resources are released at each turn boundary.

## Verification

Scripted/probe tests cover reusable turns and steering, shared JS controls,
unread receipts, configured limits, approval clamping and shared approval events,
canonical recovery without effect replay, and subtree cancellation. Nested JS
cancellation also runs with two Tokio workers. Default recursion acceptance runs
39 descendants across three levels, with three children per parent, and verifies
root cancellation closes twelve live JS environments and settles every spawn
effect. Low-retention acceptance reattaches evicted sessions for status, wait and
send, preserves terminal states, and cancels idle descendants without handles.
The restart fixture freezes a canonical dispatched boundary and reopens the store; it does not crash a live
model or perform an external effect.

The reported admission-rejection bug is not reproduced on the original one-shot
implementation: oversized context, unavailable profile and concurrency rejection
all leave no unknown effect. Core's existing dispatch call wrapper already
converts tool errors to ordinary results. Regression tests preserve that behavior
for spawn and send; runner admission errors are explicit ordinary results.

Completion regressions use `bubblepaw-runner/tests/auto_wake.rs`, `runner.rs`
(real stdio subprocess with an offline provider), `web_wake.rs` (local HTTP/SSE)
and `wake_storage.rs`. Generic archive-safe admission and bounded metadata paging
use `bubblepaw-core/tests/active_admission.rs`. Restart fixtures replace host
services over the same canonical store; they do not kill a live model process.

Control regressions use `bubblepaw-core/tests/run_controls.rs`,
`bubblepaw-runner/tests/child_controls.rs`, `child_controls_js.rs` and the runner's
`agents::control_notice_tests`. They cover valid JSON, one correction, repeated
validation failure, schema replacement and clearing, visible task/countdown
budgets, excess parallel calls, and tools-disabled final answers. Timeout fixtures
check provider-future drops, owned command/JS/subtree cleanup, fresh follow-ups,
and transient streamed partials, including interruption of a schema retry.
Reminder tests check exact boundaries, wait suppression, interaction resets,
disabling, and actual parent requests with a one-second configured interval.
No live model acceptance is claimed.

## Possible next steps

- Live-model qualification of completion wake; the current tests use scripted
  providers and local transports.

### Inherited turn controls (2026-10-15)

Model-facing spawn and continuation clamp any requested `max_tool_calls` to the
calling turn's actual remaining calls, after the JS wrapper and spawn/send call
have reserved their slots. The child's ordinary generated budget notice and tool
result countdown show the clamped value. The remaining parent wall-clock timeout
also bounds the child. Omission cannot remove an inherited bound; a stricter
explicit child bound wins. Existing turns cannot be given fresh controls by
steering. There is no default depth, concurrency, tool-call or timeout cap.

Library host adapters which launch from a tool use `start_with_dispatcher`,
`start_worker_with_dispatcher` or `send_with_dispatcher` to retain that exact
remaining-budget scope. The older context-only entry points leave tool-budget
allocation to the host; transient nested reads cannot be reconstructed from the
store. Every depth uses the same continuation, approval, cancellation and effect
paths. Listings show descendant parent ids without granting cross-parent control.
