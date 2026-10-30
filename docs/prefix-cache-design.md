# Prefix cache as a request-building contract

Status: in progress; P6 to P8 built through 2026-10-21, broader tool declarations and other protocols remain separate.

2026-10-23 chat-prefix rule: tools precede system in the cached prefix, so both
are persisted when a chat first binds its tool surface and reused across runs
and restarts. Reload never rebuilds either field. The 2026-10-30 amendment lets
LCM and classical apply rebind only the skill catalog and memory-file content
from current state, retaining the original for history. Tool declarations stay
fixed. Added tools stay
behind `tool_search`/`call_tool`, including core tools; a new chat declares the
current core set. This replaces the earlier per-run or compaction rebuild rule.
There is no separate full-prefix reset command in this checkout.

Summary apply refreshes a persisted cold catalog with current deferred-server
anchors and changes since the initial inventory. Deferred additions and removals
use names; core additions and updated definitions include full definitions and
compact JSON schemas. Deferred updates use names and server names. Covered
notices fold away with summarized history. Subsequent requests
reuse the catalog until another apply. System bytes stay fixed between applies.

The initial `submit_result` declaration (or its absence) is also fixed. A changed
run schema is delivered as a full-definition host notice, and both direct and
wrapped submissions validate against the current run contract. Providers receive
no response schema.

2026-10-19 policy update: the [approved LCM contract](lcm-contract.md) replaces
the implicit fork-to-episode fallback described in the r6 section below. New
full-prefix attempts keep that input shape across explicit route candidates;
episode-only input requires configuration. Chapter/other auxiliary fallback
selections are isolated from the main conversation's sticky route. Completing
background memory never updates the frozen serving prefix. Current verification
is recorded with the contract; the older qualification below is historical.

Status: partly implemented 2026-10-18. Durable runtime inventory notices and
adapter-aware Chat token estimation were implemented during the request-path
audit. The subsequent qualification pass adds Messages readable-input estimation
and forwards estimation through parallel/image decorators; opaque signatures and
redacted payload remain outside local semantic tokenization. Verification is
recorded below and in the LCM redesign page. Broader
declaration/control changes remain proposed. The r6 follow-up integrates Chat
episode forks into the actual background worker; the dated qualification logs were removed from the repository; the checks that run are in
[verification](verification.md). The earlier synthetic probe below remains
separate evidence, not an integrated-runtime test.

## Decision 2026-10-19: no implicit cache busts

Every prefix break must be known. When the harness itself changes the prefix
(LCM apply, tool or declaration change, config reload, route or model switch),
it says so explicitly, with the reason and the expected uncached tokens. Every
other miss is tracked per provider and model as the measured cache read minus
the client-side expected cacheable prefix, and reported as an unexplained miss.
A prefix change the harness did not declare is a bug.

Built 2026-10-19 ([cache_accounting.rs](../crates/bubblepaw-core/src/cache_accounting.rs)):

- **Wire shape.** `Provider::wire_shape` returns the exact wire body of the
  route that will serve the request (the registry picks the sticky or primary
  route, as dispatch does). The shape is a chained SHA-256: one hash over every
  non-message prompt field (generation-only fields such as `max_tokens` and
  `stream` excluded), then one per wire message, plus cumulative and total wire
  bytes. Chat, Messages and Responses implement it. Providers without it (the
  extension bridge, scripted models) are not accounted.
- **Groups.** The cache domain is provider/model. Request kind (`main`,
  `summary_fork`, `decider`) is a reporting dimension. Summary forks are
  compared with every earlier request of the same provider/model, so a fork
  reading less than the shared main prefix shows as an unexplained miss. Only
  the main loop must be append-only. Chat decider calls (`llm/` candidates) are
  accounted the same way: the decider observer rebuilds the request with
  `LlmDecisions::model_request` and shapes it through
  `ProviderRegistry::decision_wire_shape`, and each sample's metrics merge into
  its `episode_model_call` record. Deciders never declare or cause a break.
  Dedicated decision adapters (for example OpenAI logprob deciders) have no
  wire shape and are not accounted.
- **Expected cacheable** is the longest common boundary prefix with any of the
  last 16 requests in the group. A fully repeated earlier request counts its
  measured input tokens exactly; a partial repeat is prorated by wire bytes.
  The result is rounded down to the cache block (64 tokens, constant in code)
  and capped at the current input. Unexplained miss = expected − measured
  cache read. Fallback responses and multi-round continuations are recorded
  but not given an expected value.
- **Declared busts.** A main request that does not extend the session's
  previous main request is a prefix break. Its reason comes from, in order:
  a route or model switch (including fallback); an explicit declaration
  (`ContextPreparation::take_prefix_change`, which LCM apply sets to
  `lcm_apply`; `CacheAccounting::declare` for host-wide changes); or a changed
  harness component (system prompt, tool declarations, tool admission, output
  schema, reasoning effort). Anything else is `undeclared`. Each break emits
  one `cache_bust` observation with `declared`, `reason` and
  `expected_uncached_tokens`.
  A live registry generation change instead emits an informational
  `context_change` with reason `tools_reloaded` and `prefix_changed: false`;
  it does not declare a bust or refresh cold notes.
- **Where it shows.** `model_started` carries the cache group, wire bytes and
  boundary hashes (head and tail only, to bound record size). `model_finished`
  and `episode_model_call` carry expected, measured, unexplained and the break
  reason. The trace report aggregates these per provider/model. Delegation
  ledger call rows store expected cacheable, unexplained miss, break reason and
  wire bytes; attempt rows store undeclared busts and the summed unexplained
  miss.
- **Acceptance.** `crates/bubblepaw-core/tests/cache_accounting.rs` replays a
  scripted tool loop over three runs. Every main step is a pure append with no
  unexplained miss, and the declared LCM apply is the only break. The same
  replay without the declaration reports one undeclared break. Live provider
  qualification is separate.

Offline simulation 2026-10-19 over the two DeepSeek Flash fork pilot runs (request
bodies rebuilt from the chat store): every main-loop step was a pure append,
with no prefix change. Measured main hit 93.8% and 94.5% against a cacheable
bound of 94.9% and 96.6%. The gap is provider-side misses on identical prefixes
(13k and 45k tokens) and summary forks reading less than the shared main prefix
(3k and 12k tokens, cause unverified). Needed for live tracking: at
`model_started`, a cache-group key and wire-body SHA-256 at message boundaries
plus wire byte length (`prepared_bytes` is not wire bytes); at `model_finished`,
cached tokens. Expected cacheable = longest common prefix with an earlier
same-group request, rounded down to the provider's 64-token block.

## ContextState request diagnostics (P1, 2026-10-21)

`bubblepaw-core::ContextState::build` takes an assembled `ModelRequest` and
returns that same request with diagnostic metadata. It does not render, reorder
or edit requests, move context writers, set cache breakpoints or write SQLite.
The root loop (including child sessions), summary forks, episode/chapter workers
and LLM deciders build immediately before dispatch. Adapter forwarding, retries
and transport continuations forward the already built request.

The comparison lineage is the observation's `session_id` plus `request_kind`.
Root requests retain their lineage across turns; each child has its own session.
Auxiliary operation kinds are separate; LLM decider kinds include their stable
question names, separating approval decisions from chapter splits. Each lineage retains only its immediately
previous tier fingerprints and per-history-message hashes, independently of the
legacy provider/model cache-hit search.

The tiers are `frozen` (system prompt and the declared tool set),
`cold` (explicitly annotated applied-view prefix messages), and `history` (the
remaining messages). Cold provenance comes from the exact saved view and follows
immutable fork captures, rather than being inferred from text. Ordinary history
has an empty cold tier. The declared tool set is the active `tools` followed by the
inactive `disabled_tools`, in order. The frozen tier hashes the active and inactive
lists separately, so moving declarations from `tools` into `disabled_tools` is a
`frozen_change` (the enforcement round declares `tools_off_enforced`), as is any
other change to the declared set. Adapters then pick the tool choice from the
active/disabled split. Hashes use compact JSON with
recursively sorted object keys, preserving array order. Message content, tool
calls/results and provider replay state participate; durable metadata and host-only
tool approval/read-only flags do not. These are provider-independent content
fingerprints, not wire-body hashes or provider token estimates.

Every built request emits a metadata-only `context_built` observation when tracing
is enabled. Its fields are `request_kind`; `frozen`, `cold`, `history` (each has
`sha256`, `bytes`, `message_count`); `classification.kind`; `declared`;
`declaration_reason`; and `invalid_cold_indexes`. Observation scope carries the
session and request identity. Classification is `first`, `append`,
`frozen_change`, `cold_change`, or `history_rewrite`, in that precedence order.
A rewrite also carries `classification.first_differing_index`, zero-based within
the history tier; truncation differs at the new history length. Equality counts
as append. Invalid cold annotations are counted without changing the request.

There is one ledger per store (`Store::cache_accounting`), the one prompt-cache
ledger shared by every request of the host that owns it, traced or not. The
root loop, summary and chapter forks, deciders and fresh side calls all classify
against it; a trace only receives their events. `CacheAccounting::declare`
supplies per-session or global declaration epochs to both systems. The tier event calls a change declared only when an explicit new
epoch covers that lineage; first/append events have `declared=false`. Existing
wire-cache accounting still reports its inferred component/route break reasons
and existing `model_started`, `model_finished`, `cache_bust` and
`episode_model_call` consumers keep their fields.

Accepted exclusion (2026-10-21): the dedicated OpenRouter, OpenCode and OpenAI
decision adapters send `DecisionRequest` input/state and question structures,
not `ModelRequest` messages. Their servers assemble the prompt, so these paths
stay outside ContextState. No client-side tier mapping is planned for them.

P1 validation on Linux passed formatting, strict workspace Clippy, the full
core suite, focused runner tests and the file-size ratchet. Three candidate
captures agree on all 40 request keys. Against three `c3b44a6` captures from the
same workspace, `compare.py` reports totals `2,2,2 / 2,2,2 / 1,1,1` (baseline
rows, candidate columns). Baseline repeats themselves reproduce the Messages
root 8/9 differences: child-completion notices arrive before root 8 in one run
and after it in the others, moving their placement and an existing cache marker.
The remaining difference is chat summary-fork 2's documented view-apply race.
All child requests and all chapter-worker requests agree. P1 was accepted on
2026-10-21: the B1/B2 root differences are baseline nondeterminism. The original
comparison totals remain recorded above. These are scripted local HTTP captures,
with no live-model acceptance.

## Tail reminders (P2, 2026-10-21)

`ContextState::remind` queues a kind and text within a session/request-kind
lineage. Queueing alone starts no call. The runtime uses `build_persisted` to
append one combined Generated user message, commit it, and invoke the ordinary
`build` classifier once. Each entry uses the single wrapper
`<harness kind="KIND">TEXT</harness>`. The latest queued value of each kind wins;
text identical to that kind's last sent value is suppressed, including after
restart. Existing history messages are never moved or reinserted. The flush is
after the latest accepted user input or tool results. Failed preparation,
request admission or storage leaves the queue available for retry.

Runtime inventory uses a stable snapshot kind. With `max_tool_calls` set, the tool budget
reminder is sent at most twice per run: once at run start with the limit and the
remaining count, and once as a warning when remaining calls drop to
max(2, 20% of the limit) or fewer. Its kind is `tool_budget:<run id>`, so a new run
with the same limit still gets its start notice. A run without a limit gets none.
Child/process completions and question answers use identity-scoped event kinds.
Incremental memory, skills and config diffs also have event identities: two
independent changes must survive one flush, and a later recurrence is a new
change. Ordinary runtime inventory contains live facts only; the full runtime
snapshot is sent once after an LCM apply (see P4 below).

Delivery recovery reads structured metadata, never the rendered new reminder
text. An additive `reminder_metadata(session_id, record_id, data)` table stores
version, request kind, reminder kind, text SHA-256 and delivery identities beside
accepted inputs and Generated messages. Normal metadata does not duplicate the
notice body. Accepting an input and its metadata is atomic; flushing commits the
combined message, metadata and source-input acknowledgement in one transaction.
Staged `reminder_pending` inputs remain canonical until that commit. No trace
records are stored in SQLite, and the existing schema version stays unchanged.

Host notices reach the model only as structured reminders: the string
`take_notices` methods, legacy string wake callbacks and `[host notice]` text
building are gone. Two immutable migration watermarks restrict recognition of
`[host notice]` text to records already present when this feature first opens
the store. New unannotated text cannot claim legacy delivery. If an old queued notice is flushed after the
upgrade, its verified old source text and record ID travel as migration-only
`legacy_sources` provenance; this small compatibility case retains an extra copy
of the old text. Recovery can inspect that provenance after restart without
parsing the new combined message. Existing Generated origin remains available
to host consumers. Dedicated server-assembled decision
APIs remain excluded as documented above.

## Cold tier from the LCM view (P4, 2026-10-21)

An LCM apply changes the cold tier. Under the 2026-10-30 amendment it may also
rebind the skill and memory-file content of the system prompt. Unchanged content
still permits a `cold_change`; changed prompt bytes produce a `frozen_change`,
declared `lcm_apply`. Tool declarations remain fixed. The earlier P4 guarantee
of byte-identical system prompts across every apply is superseded.

The view also no longer embeds a runtime inventory. When the
runner's `SummarizingContext` consumes the `lcm_apply` declaration it queues the
full runtime note with `ContextWriter::remind_fresh`, which bypasses the unchanged-text dedup once, so the
facts appear exactly once, as `<harness kind="runtime_inventory">` in the tail.
There is one structured reminder path. Wake recovery still recognises old
`[host notice]` and `[question answer:...]` text, but only in records written
before this change.

View-dependent instructions are cold notes. The runner's `SummarizingContext`
writes them from the hook with `ContextWriter::cold_note` (today only
`history_lookup`), and `build_persisted` places them as one
Generated system message right after the last cold message, ordered by kind and
then insertion, each in the `<harness kind>` wrapper. The served notes for a
lineage refresh only at its first request, at a newer declaration, or when the
cold tier is empty; an undeclared change is held back, so the cold tier stays
byte-stable between applies. Notes are derived from the view each build; nothing
new is persisted. The memory snapshot and skill catalog stay in the system prompt.

Views saved before P4 (a non-empty `system_prompt`, or a bare lookup message in
the prefix) are served the old way: their stored system prompt and prefix,
without cold notes.

The view is applied in one place, `SummarizingContext::serving_view`
(`summarization_context.rs`), used by the live hook, the episode trigger and
replay. Cold notes are placed only by core (`ContextState::place_cold_notes`,
`cold_notes.rs`), so the same served lineage cache applies everywhere. Replay
builds each request it sends through its own `ContextState` (`prepare_step`), so
a compaction request carries the cold notes as a live request does.

## Static Messages `system` (P5, 2026-10-21)

On the Anthropic Messages wire, the top-level `system` field holds only
`ModelRequest::system_prompt` (the frozen system prompt; for a legacy view, its
stored prompt). Every `Role::System` message in the list, including the LCM view
blocks and the cold-notes message, renders in place as a user text block and
merges with adjacent user content, as consecutive user turns already did. An apply
therefore leaves `system` byte-identical, and the change starts at the first
message. `tests/cold_tier.rs` checks this on the real apply path. Chat and
Responses are unchanged: they already keep system messages in position.

In a user turn, `tool_result` blocks always come first, so a system message
between a tool call and its results renders after the results, never before
(Anthropic rejects text ahead of a result).

Cache breakpoints sit at the tier ends. `ContextState::build` returns
`CacheTiers { cold_end, previous_len }` as message counts: `cold_end` is one past
the last cold message (the cold-notes message included), and `previous_len` is
the length of the lineage's previous request, set only when the request is an
`append` of it. `build_context_request` copies them into
`ModelRequest::cache_tiers`; forked side calls get the fork's tiers (P6), fresh
side calls and external callers leave it `None`.
Nothing is inferred from ids or roles. The Messages adapter places at most four
ephemeral breakpoints in one function (`provider/messages_cache.rs`):

1. the frozen end: the system block, or the last tool when there is no system
   prompt (Anthropic's prefix order is tools, then system);
2. the cold end: the last user-turn block holding only messages before
   `cold_end`;
3. the lukewarm point: the same for `previous_len`, the previous request's tail;
4. the last block of the request.

Without tiers only (1) and (4) are placed. Message breakpoints go on user-turn
blocks only. An apply therefore invalidates from the cold tier onward, and an
ordinary step reads the cache up to the previous tail. Chat and Responses get no
markers; `tests/wire_order.rs` checks that their order is unchanged.
`WireShape` ignores `cache_control`, so a moved breakpoint is not counted as a
prefix break; the signed-prefix mock in `tests/research_live400.rs` likewise
compares prior messages with markers stripped. The previous tail lives in the
in-memory lineage, so the first request after a fresh `Harness` has no (3).



## Side calls as forks (P6, 2026-10-21)

A side call that runs on the chat's model reuses the chat's cache by sending the
parent's exact prefix. `ContextState::fork(request, session, parent_kind,
fork_point, kind, cold, task)` takes the parent's request cut at, or extended with
completed canonical rows to, `fork_point`, appends the optional `task` as a tail
reminder (`<harness kind="...">`, a generated user message) and classifies the
prefix against the parent lineage (`root`), measured with the parent's own cold
split (a capture's cold annotation can omit the cold-notes message): equal frozen
and cold fingerprints and a history-hash prefix give `append`. It sets `cold_end` and `previous_len` (the
parent's previous end, capped at the fork point), so the fork reads the block the
parent wrote. It records nothing: the parent's lineage and every other lineage are
unchanged, and the fork transcript is never merged into history or persisted.
Tools (including any main-task `submit_result`) and tool choice stay the parent's, so the
frozen prefix is unchanged. For a native parent already declaring `call_tool`,
the appended control requests
`call_tool({name:"submit_summary",arguments:{summary:"text"}})` (or
`submit_chapter` for chapter memory). These synthetic targets are fork-local;
they are never registered or advertised as real tools. Exactly one matching
return call is extracted and its summary string follows the existing memory
validation and chapter `Related:` parsing. Providers receive no response schema;
the main result contract is the stable `submit_result` input schema. Plain text
remains valid, including for JS parents. Forks refuse `submit_result` without
executing it, whether called directly, through `call_tool`, or inside JS.

Every other declared tool call, malformed return or set of multiple calls gets an error
result without execution and is retried once. Native refusals repeat the
synthetic return instruction; JS refusals say "No tools in this task; answer in
text." A second wrong call fails as `tool_call_returned` (`dispatch_fork` in
`src/episode_models_fork.rs`). Usage covers both attempts, and the branch
transcript is never merged into the main history.
Undeclared tool names are rejected by the provider adapter before fork dispatch.

Byte equality holds for the content, not for the markers: the parent marks its
frozen end, cold end, its own previous tail and its last block, while the fork
marks frozen, cold, the parent's last block and its own last block. Tests compare
the prefix with markers removed and check the parent's last block is marked.

Call sites:

- **Summary fork** keeps its measured control (a system branch control and a
  user return control after the cut prefix) instead of a tail reminder, and now
  passes the real tiers.
- **Chapter worker** forks the latest confirmed main capture cut at or extended
  to the chapter's last message (`SummarizingContext::chapter_fork`) with the
  chapter prompt and input as a `chapter_task` reminder, only when the first
  available chapter route equals the capture's model with the same reasoning
  effort and output limit (an unset role limit matches). Lineage
  `chapter_fork`, trace kind `chapter`.
- **Episode-only summary** (`summary_input=episode`) stays fresh.
- **Deciders** stay fresh; they almost never share the chat's model or settings.

A fresh side call is built with `ContextState::build_side` and is always
declared, so its miss is never unexplained. The reason is explicit or derived
from the parent's last request: `side_call_no_capture` (no parent or no confirmed
capture), `side_call_other_model`, `side_call_settings` (reasoning effort or
output limit differ), `side_call_episode_input` (explicit episode-only input) or
`side_call_unforked` (matches the parent but is not forked, for example a
classical summary, or a chapter fork refused for an unsupported protocol or the
byte limit). Dedicated decision APIs are unchanged.

## Host context through ContextState (P7, 2026-10-21)

Hosts add per-request dynamic context through `ContextWriter`, handed to the
preparation hook `ContextPreparation::prepare(call)` as `PrepareCall.writer`:
`remind(kind, text)` queues a tail reminder (P2 rules: latest per kind, sent
only when changed, persisted), `remind_fresh` sends it once even when unchanged,
and `cold_note(kind, text)` stages a note for the end of the cold tier (P4
rules). The default `prepare` passes a request through, and a `Compact` mode
fails unless the host implements it. The cold notes are cleared before the hook
runs (`clear_cold_notes`), so notes a hook stages reach the build. See
[host-context](host-context.md).

`prepare_with_writer` records the frozen tier before and after the host hook
through `ContextState::note_preparation`. The harness then restores the persisted
chat tools and system prompt, including clearing injected disabled declarations.
Attempted hook edits therefore cannot replace the provider's frozen prefix.
Tools-off shaping happens after restoration and retains the wire declarations.

`RunOptions.system_prompt` and `system_prompt_resolver` select the initial chat
prompt. Later runs reuse the persisted text. Hosts publish subsequent memory or
skills changes through notices rather than replacing that prompt.

An early desktop host appended the current project
and the owned processes to the system prompt on every request, and its run
prompt carried the project name and folder, busting the whole cache on every
switch or process state change. It now sends `remind("project", ...)` and
`remind("processes", ...)` and leaves the system prompt static.

Checks (Linux, scripted): `tests/host_writer.rs` (4 tests) covers tail placement
and dedup, the undeclared hook edit, the declared resolver change and the
compaction focus reaching the next tail. `cargo fmt --check` and strict workspace
Clippy pass; the core suite passes, including the file-size ratchet; the runner
suite passes except `agents_repl`'s `registered_nested_child_js_obeys_depth_and_parent_cancel_cleans_subtree`,
a cancel-timing race that also failed on a clean `HEAD` export (1 of 4 runs). No
live model acceptance.

## Request log and unexplained cache misses (P8, 2026-10-21)

Every request `ContextState` builds gets one row in the serving store's
`model_requests` table: session, lineage (`request_kind`), per-lineage `seq`,
time, the three tier fingerprints, classification, `declared` and
`declaration_reason`, provider and model, cached and uncached input tokens once
usage arrives, the predicted cached tokens, `since_previous_ms` and `warned`.
For offline trace analysis the row also holds (all nullable, 2026-10-21):
`cache_write_tokens` (Anthropic `cache_creation_input_tokens` or another
provider's cache-creation count; null when the provider has no known cache
lifetime and reports zero, since `Usage` cannot tell zero from unreported),
`output_tokens` (with usage), `reasoning_tokens` (where reported),
`reasoning_effort` as requested (null: the adapter's default, as on root
requests), `breakpoints` (the `cache_control` wire positions the Messages
adapter sent, as compact JSON such as `["system.0","messages.4.1"]`; null for
adapters that place none), `cache_lifetime_ms` and `likely_expired` (null when
the lifetime or the gap is unknown). Messages reports its breakpoints under
`_bubblepaw_cache_breakpoints` in the response's `provider_state`; the runtime
removes the key from root responses before the message is stored, while side
and fork responses that are kept carry it. An older serving.db gains the columns
by `ALTER TABLE ADD COLUMN` at open; its rows keep nulls.
In run12 (scripted mock, 20 rows per adapter) a row's column contents summed
322 bytes on chat and 359 on Messages (run11: 318 and 318), so the added columns
cost about 4 bytes on chat and 40 on Messages, mostly breakpoints; serving.db
stayed at 217 KB. The mock route has no known cache lifetime and reports zero
writes and no reasoning tokens, so those columns were null there. Retention is
unchanged: rows and tiers follow `telemetry_retention_days` (default 30) and the
per-root policy; there is no size cap.
Frozen and cold tier contents (the exact canonical bytes behind the hashes) are
kept once per distinct hash in `prompt_tiers`, so an unexplained bust can be
diffed afterwards. History is not stored; it is in the chat file. The log is
disposable telemetry: housekeeping deletes rows by the same per-root and host
`telemetry_retention_days` cutoffs as `telemetry`, then deletes tiers no row
references. No chat-store schema changes.

A row opens at `build` (seq and `since_previous_ms` also go on `context_built`)
and is written when usage arrives (`ContextState::finish_request`), or without
usage when the lineage's next build supersedes it, at `release_session`, or
when the state drops. A row is one buffered `write_serving` call, so it joins
the open serving transaction and adds no commit. The state holds a weak store
handle, attached by the runtime (`attach_request_log`); stores without a
serving view (in-memory, plain) log nothing.

Prediction needs no tokenizer. The unchanged prefix in canonical bytes is zero
for `first` and `frozen_change`, the frozen tier for `cold_change`, frozen +
cold + the previous history for `append`, and frozen + cold + the history before
`first_differing_index` for `history_rewrite`. Expected cached tokens are
`input_tokens × unchanged / total` bytes (input includes cache reads and writes).

A warning fires on an undeclared non-append, or on an append whose expected
cached tokens are at least 1024 (below that providers do not cache) and whose
actual cached tokens fall short by more than max(128 tokens, 10 %). It goes to
stderr and, when tracing is on, to the trace as `cache_miss_unexplained` with
`session_id`, `lineage`, `seq`, `cause` (`undeclared_change` or
`cached_shortfall`), provider, model, classification, `declared`,
`declaration_reason`, expected, cached and uncached tokens, `since_previous_ms`,
`cache_lifetime_ms` (Anthropic: 300 000, the 5-minute default; unknown for
other providers) and `likely_expired` (gap past the lifetime). Expiry
prediction and UI are out of scope.

Root requests finish in the runtime; fork lineages in `dispatch_fork`, deciders
in `LlmDecisions::decide_using` and fresh side calls (summary, episode) in
`EpisodeModels`, each with the response's usage and selected provider. Side
calls log whether or not tracing is on: the registry attaches its usage store
to the route (`ContextRoute::attach_store`), which takes that store's ledger,
so a traced or untraced fork continues the root lineage of the same ledger the
root loop uses. A call that fails without a response keeps its row
until the lineage's next build and is written without usage. In run11 (before
side-call finishing) decider and Messages summary rows had no usage.

Tolerance from the bench (run11, scripted mock that reports cached tokens by
longest common prefix in 64-token blocks): appends with at least 1024 expected
tokens had cached/expected between 0.959 and 1.41 on chat (9 rows) and 0.971 and
1.003 on Messages (6 rows). The largest shortfall was 4.2 % (144 tokens), inside
max(128, 10 %). Both adapters logged 20 rows (root 11, summary 3, decider 6,
matching `context_built`) with zero `cache_miss_unexplained`. `prompt_tiers` held
7 and 8 blobs (16 and 20 KB); serving.db grew from 160 KB (run9) to 213 and
217 KB. `storage_turn_measure` keeps 5 transactions (4 chat, 1 serving) against
8f34a56; its WAL grows 32 960 bytes (1 363 816 to 1 396 776). Unit tests in
`context_log.rs` and `tests/prompt_log.rs` cover the warning, the row, the blob
and retention paths; the bench did not trigger a warning. No live-model acceptance.

## Outcome

Within a serving view, an ordinary next request should retain the preceding
request's compatible model-input prefix and append new conversation/control
records. A summary should be able to fork that same prefix, select an exact
episode, and return memory without changing the main conversation or executing
tools. LCM apply is a deliberate change of the serving prefix, not a side effect
of finishing a background summary.

This is a property of the final provider request, not just of the Rust history
vector. Model, endpoint, wire protocol, tool definitions, rendering of old
reasoning/tool turns and provider routing all matter. Client-side equality can
be tested; a server cache hit is still best effort and must be measured.

## Current source audit

| Area | Current behavior | Consequence / required change |
|---|---|---|
| Memory and skills | File content and skill catalog are frozen between compactions; changes become durable generated notices. Applied compaction rebinds their snapshot from current state. | The resolved base prompt remains fixed. Reopen restores the latest snapshot plus subsequent notices. |
| Runtime state | Initial inventory and actual changes append durable Generated-user reminders (`<harness kind="runtime_inventory">`). Unchanged inventory is quiet, also across process restarts: the first preparation seeds the comparison from the latest stored notice, using structured reminder metadata. Prior notices retain their positions. After an LCM apply, the full snapshot (saved key names and REPL generations) is sent once as the `runtime_inventory` tail reminder; the view prefix carries none (P4). Explicit compaction publishes the same full form. Ordinary steps publish only live facts: running, waiting or backgrounded REPL cells, commands that are running or backgrounded or exited without their terminal status returned (marked `unread`), and agents that are running or have an unread result. A finished command or agent whose result a tool already returned is not listed. With nothing live the notice is one fixed line, so it stays quiet. | The old disposable system tail is removed. Chat preserves these notices in position; Messages no longer hoists runtime inventory into its system block. Local transport fixtures verify this layout; fresh live cache qualification remains separate. |
| Chat adapter | Historical `Role::System` messages remain in their conversation positions. | A tail system/control message can preserve earlier input. Test on the actual route. |
| Claude Messages adapter | Only the system prompt goes in `system`; historical `Role::System` messages render in place as user text (P5). | A late system message no longer rewrites the opening block. |
| Explicit cache markers | Messages places up to four ephemeral breakpoints at the tier ends: frozen end, cold end, previous request tail and last block (P5). | Qualify provider-native cache placement/retention alongside prefix equality; do not assume a stable request alone guarantees the desired cached tail. |
| Responses adapter | Base instructions use `instructions`; historical system messages stay in `input`. | Preserve their positions. Qualify the provider's accepted control role and cache behavior rather than assuming Chat rules apply. |
| Tools-off rounds | For ordinary runs, the first final-budget round keeps `tools` unchanged, queues the `tools_off` reminder and sends no tool-choice change. Only if the model still calls a tool does an enforcement round move definitions from `tools` to `disabled_tools`, keep them on the wire in order, and send the provider's tool choice off (`none`), declaring `tools_off_enforced`. Call parsing accepts only active tools. Required-result runs instead retain `submit_result` and its access wrappers after exhaustion; ordinary targets remain blocked. Missing-result reminders also leave declarations unchanged. | The reminder round is an `append` with the same declared set and tool choice. The enforcement round is a `frozen_change`, because the active/inactive split moves. Changing API `tool_choice` can lose reuse on the tested route (controlled fork below). Chat and Responses `tool_choice: none` is not measured yet. |
| Execution modes and live tools | Native core declarations or the opt-in `js` declaration are persisted for the whole chat. Live tool publication updates dispatch and discovery. | Notices announce changes; apply refreshes the cold catalog. Declarations stay fixed; only the skill and memory-file prompt content may be rebound at apply. |
| Chat reasoning replay | Default `tool_loop` remains configurable; the DeepSeek Flash free/Go worker models explicitly select `all`, matching their tool-use contract. | Preserve compatible native reasoning across completed turns and length continuations; retain adapter/model/route guards. |
| Episode summaries | Default `summary_input = "fork"` captures the final main request and appends branch-local system/user controls through the selected complete episode boundary. The opening prompt, declarations and compatible native reasoning are preserved. | Same source route and provider generation; no branch tool dispatch or main mirror mutation. Compact episode input remains explicit/configurable fallback with categorical diagnostics. |
| Background LCM / apply | Gathering is independent; apply freezes ready chapters/episodes plus an exact uncovered suffix until the next apply. The r5 repair removes masked raw refilling; oversized completed batches can be summarized whole under pressure. | Gathering must not update the live index, chapter descriptions or old raw results in the main prefix. A complete call/result boundary and actual advancing summary coverage are required for replacement. |
| Native thinking at apply | Replay compatibility currently checks adapter/model/route; earlier live work proved that changing signed-thinking prefixes can cause rejection. | An apply boundary needs its own native-thinking qualification. Complete call/result groups alone do not prove that retained signed state remains valid under a changed earlier prefix. |
| Provider fallback | Free-first routing records actual route provenance and sticks to the successful route per source session. Auxiliary retries stay within the actual candidate. Token admission considers the largest known rendered input across still-reachable alternatives. | Free and Go are separate cache domains. A resumed session with Go reasoning must not be sized only as a cold free-route request. |
| Token admission | Chat and Messages estimates share their actual message/tool/schema renderers. Core supplies the captured provider generation through preparation and UsageContext; parallel/image decorators forward estimation. Source cadence separately counts readable Chat/Messages reasoning. | Readable replayed thinking counts; archival SSE journals and opaque signatures/redacted payload are not semantic tokenizer text. These are local estimates, not exact provider token counts. Calibration cannot lower the conservative local bound; opaque output retains its measured residual without doubling represented thinking. Detected continuation/fallback usage is not matched to the wrong original request shape. |

Source: [runtime](../crates/bubblepaw-core/src/runtime.rs),
[Chat](../crates/bubblepaw-runner/src/provider/chat.rs),
[Messages](../crates/bubblepaw-runner/src/provider/messages.rs),
[Responses](../crates/bubblepaw-runner/src/provider.rs),
[memory](../crates/bubblepaw-runner/src/memory_files.rs),
[skills](../crates/bubblepaw-runner/src/skills.rs),
[live capabilities](../crates/bubblepaw-runner/src/live_config.rs), and
[episode requests](../crates/bubblepaw-runner/src/episode_models.rs).

## Request layout

Keep the opening instructions small and stable. Put initial model/capability
information into one ordered, generated session-start record. New effective
state is a new record at its actual position; never replace the old record.

The byte-level prefix capture tool that produced the results below was removed with `experiments/` before the first public release (2026-10-22); the per-request `model_requests` log now records cache reads and writes.

```text
Frozen base instructions + initial memory/skill catalog
Frozen advertised tool declaration(s), in stable order
Session-start control record: model reference, capability/catalog identity
Conversation / tool calls / exact tool results
Control change at the time it occurs
Further conversation
```

Model selection is still a transport field: a control record does not make an
API route change cache-compatible. Likewise a notice that a tool is unavailable
does not enforce admission; the local dispatcher does. Descriptions and state
are separate from execution permissions.

For Chat, the control record can be an in-position system message. For Claude
Messages, use the `<harness kind>` reminder bridge in the message stream, because
that protocol has a top-level system block rather than late system turns. The base
instructions do not describe the wrapper today; each reminder carries its own
`<harness kind>` tag. Responses needs its own
qualified in-position representation. Do not make an adapter hoist a late notice
into the initial instructions for superficial cross-provider uniformity.

A real base-instruction/schema replacement, incompatible model/endpoint switch,
history edit, rewind or LCM apply starts a new prefix generation. Record the
reason and retain any common earlier prefix where possible. Do not call these
operations append-only when they are not.

In particular, establish how each provider crosses this boundary with native
thinking or stateful cursors. Do not silently reconstruct a signed tool loop,
reuse an incompatible native cursor, or assume a normal host notice closes the
old loop. Qualify the provider-supported checkpoint/new-context transition with
a signed live fixture before promising Claude compatibility through apply.

## Summary fork

Implemented for Chat in r6 and corrected by the 2026-10-19 contract.
`context.lcm.summary_input = "fork"`
is the default; `"episode"` selects the earlier compact source method explicitly.
The fork uses the captured main model/route and its generation settings, rather
than migrating the request to another model in `roles.summary`. That role still
controls the optional summary output allowance and configured repeats. Full-fork
route retries start from the captured successful main route and only its explicit
provider fallbacks; every candidate retains the full-fork request shape.
The semantic summary length target stays prompt guidance; it is not converted
into a hard thinking-plus-answer output cap. With no explicit role override,
the original main output setting (including no limit) is retained. Admission
reserves that actual allowance, or configured `context.lcm.output_reserve_tokens`
when no hard output limit exists.
Chapter synthesis and Jev decisions keep their compact independent inputs.

The core observes the final logical request after admission, tool-surface and
steering changes and before provider dispatch. The captured provider generation
reapplies its same stateless Chat replay/image renderer; the source mapping uses
that renderer's actual positions, including expanded reasoning rounds. The
capture is not a raw HTTP dump and does not capture the last physical request
inside a provider's length-continuation loop. It retains the warmed starting
prefix and reconstructs the completed canonical suffix without replaying effects.

Each session keeps an immutable completed capture shared with in-flight forks.
A new main request is staged while pending; gathering continues using the last
completed prefix and confirmed route. Only a successful response promotes the
new capture and its actual fallback route. Failed calls do not promote an
uncertain route, and there is at most one pending capture alongside the completed
one. A first pending call without a confirmed capture leaves the range uncovered
and reports that a full fork is not ready; it does not silently select episode
input. Pre-dispatch cold pressure candidates are labeled separately.
The builder trims any future canonical messages, verifies the selected source
remains exact, and appends committed messages through the selected end when the
capture is older. The selector maps only the selected episode's source/call IDs, including original
message origins and `is_typed_user` classification. The branch instructions and
return/continuation controls are explicitly excluded from historical events and
human requests. Earlier history stays present as context. New background summaries cannot change
the main serving prefix. Idle hidden-child cleanup waits for its final background
drain asynchronously before releasing the capture, preserving resumed captures.

The branch leaves API tool choice and declarations unchanged. Its appended
system control says tool execution is unavailable; returned tool calls are
rejected with their reported usage and never executed. Snapshot/cursor and fork
control records never enter canonical main history. Fork dispatch bypasses main
sticky-route/session-mirror mutation and uses the successful main fallback route.

Episode-only input must be explicitly configured. Missing source/capture,
unsupported protocols/native schema, unavailable mapping or exhausted full-fork
byte/window admission fail without advancing coverage or changing the served
view. Route fallback retains the fork; it does not select episode-only input.
Explicit episode mode keeps complete canonical tool messages
and arguments by default (`tool_evidence_tokens = 0`); a positive value explicitly
opts into the older lossy shared budget. It still avoids reading external spill
files and does not copy opaque provider reasoning. Unknown capacity alone does not invent a rejection
threshold. Optional metadata tracing records `episode_fork_fallback` reasons and
`episode_summary_input` / `episode_model_call` with actual mode and usage/cache
counts. Current Messages/Responses callers must explicitly choose episode mode;
this does not
establish signed Claude/Messages apply compatibility.

The following design contract explains those choices; its broader multi-protocol
qualification and stable-tool-declaration work remains outstanding.

Capture the prepared provider input at the serialization boundary, after replay
and tool-surface decisions. Retain one immutable in-memory snapshot per active
session, shared with an in-flight summary by reference. Do not persist a copy of
the entire prompt on every round or build another authoritative transcript.

The snapshot needs the actual provider/model/protocol identity, unchanged system
and tool fields, the rendered ordered blocks, the serving-view identity, and a
mapping from canonical source IDs to rendered positions. This mapping is needed
because canonical IDs are not generally sent as message IDs in the API body.

Forking copies that prepared input and appends a branch-local control instruction:

```text
Purpose: write episode memory; do not continue the workspace task.
Summarize source range {first ID} through {last ID}, inclusive.
Rendered positions and exact source/call ID mapping: {...}
Earlier material supplies context; only the selected range supplies new events.
Preserve corrections, evidence, verification limits and unresolved work.
No tool execution is admitted in this branch. Return the episode summary.
```

Append a separate request to return the summary when the provider requires it.
Do not replace the initial system prompt with the summarization prompt; do not
remove the tools, change their order, or change presence/value of `tool_choice`
just for this branch. The local summary path never dispatches returned calls.
If a model returns a call instead of a summary, record that as a failed summary
attempt with its usage; do not execute it.

The default experiment should retain the main route and compatible generation
settings. A smaller summary output allowance worked in the controlled probe;
changing reasoning settings or adapters requires independent qualification.
An Opus prefix cannot warm a DeepSeek Flash or Haiku request. A different summary model
remains an independent compact request, even if its history text is identical.

### Source frontier and concurrency

The summary owns an immutable source boundary and view snapshot. It cannot gain
later events because the main worker advances while the request is in flight.
When gathering lags, omit all messages after the selected episode end, rather
than showing future events and hoping the summarizer ignores them. Keep complete
tool call/result groups and preserve original IDs in the appended mapping.

Prefer a naturally warmed main request that already contains the episode. A new
tool result may not yet have been sent by the main worker; it is a new suffix,
not cached input. Waiting for the next natural main dispatch can improve reuse,
but must not stall the main worker. On idle/shutdown or insufficient compatible
coverage, the explicit bounded episode method remains available. Do not replay
workspace effects to manufacture a warm prefix.

If the necessary raw range has already been compacted out of the snapshot, use
canonical range input. Never pretend a chapter summary is the missing original
episode. If adding the selector/output reservation would exceed the actual model
window, use bounded input or a shorter compatible captured prefix; do not add an
unrelated cosmetic threshold.

Fork state is isolated: its output is stored as episode memory with ordinary
usage/evidence, and its control messages never enter the main chat. Existing
session deletion/shutdown cleanup releases captures and cancels gathering.

## Where whole-context reuse is appropriate

Episode gathering is the first candidate because the selected source was just
processed by the main worker. It keeps the configurable 24k/32k source policy and
roughly 2k summary guidance; the fork changes input construction, not cadence.
It does not inherently add calls compared with one summary per episode.

Chapter synthesis still consumes ordered episode summaries with exact evidence
and final state. Whole main history is not automatically its best input. Chapter
decisions now have bounded accumulated chapter state and a candidate episode;
the separately requested Jev provider can evaluate that compact state. Different
models do not share a prefix cache. Do not force every auxiliary operation into a
large main-model fork merely because one episode fork is cheap.

LCM apply remains explicit at serving pressure (200k host / 100k workers, limited
by physical model capacity and output reservation). Gathered summaries can wait
while the existing main prefix continues earning cache hits. Apply replaces old
raw content with the frozen chapter/episode projection, retrieval guidance and
recent tail. New gathered material does not mutate an already applied prefix.

## Measured probe and cost model

The actual BubblePaw coding pilot points to ordinary agent requests as the first
priority. Its 111 main calls used 2,260,530 uncached input tokens; six auxiliary
LCM calls used 31,099. Thus about 98.6% of uncached input in this one run came
from main calls. Summary forks alone cannot repair that cost. Several late main
calls reused only 2,432 tokens, interspersed with almost fully cached calls.

The metadata meter recorded constant tool-schema byte length, not schema hashes
or complete wire bodies. It cannot establish that all earlier blocks were
identical or attribute those misses to LCM, tool-loop rendering or upstream
routing. The separate synthetic experiment also missed on one identical request.
Use controlled wire-prefix comparisons to separate client mutations from server
cache misses before assigning a cause. Persisted context-view row count is not
an apply-event count. Detailed aggregates are in `flash-bench/prefix-analysis.json`
in the usage-analysis workspace.

The controlled DeepSeek Flash/Go experiment on 2026-10-18 used a synthetic approximately
51k-token worker prefix and planted success/failure/correction/source facts. It
is a transport-pattern test, not an integrated BubblePaw continuation benchmark.

| Method | Input | Cached input | Uncached input | Output including reported reasoning |
|---|---:|---:|---:|---:|
| Cold bounded episode | 4,210 | 0 | 4,210 | 1,956 |
| First user-trailer fork | 51,611 | 50,944 | 667 | 1,635 |
| Fork changing `tool_choice` to `none` | 51,298 | 0 | 51,298 | 1,560 |
| Appended system/control fork | 51,601 | 50,944 | 657 | 938 |
| Main continuation after system/control fork | 51,235 | 50,944 | 291 | short acknowledgement |

The system/control fork retained all 16 planted rubric items on direct review,
with one minor stdout/stderr attribution slip. An earlier byte-identical auto
fork repeat also missed cache. These samples cannot establish a stable hit rate
or show that `tool_choice` alone caused every miss.

The subsequent literal artifact audit confirmed that both forks retain all 63
original native messages and tools. The auto fork and its repeat have identical
161,483-byte client bodies, yet report 51,611 versus 51,575 prompt tokens and
50,944 versus zero cached tokens. That unexplained input-accounting difference
prevents treating this as a controlled test of cache residency. Historical HTTP
headers and provider-rendered token streams were not captured. The fixture's
historical reasoning fields are empty, so it does not qualify real reasoning
replay. Warm-to-fork also changes the trailer and output budget, 256 to 2,000.

In the separate monitored malformed-stream BubblePaw run, 118 ordinary main
transitions with unchanged combined system content reported 6,580,864 cached
tokens out of 6,789,187 input (96.9%). The 28 transitions with changed combined
system content reported 65,792 out of 1,857,703 (3.5%). The opening system and
tool schemas remain stable. Request 129, previously described as evidence of
unreliable caching, belongs to the changed-system group; a generated user-role
process notice also appears immediately before it. These are concrete client
confounds, not proof of a provider cache failure or proof that system messages
are hoisted. The synthetic appended-system fork did reuse its prefix. Diagnose
the effective route rendering before assigning a cause or general hit rate.

Those figures describe the frozen baseline with the old mutable snapshot. The
source correction now uses the durable notice contract above. It does not
retroactively establish a repaired live hit rate. Focused fixtures cover ordinary
state changes, unchanged prefixes, child isolation, idle-cache release, restart,
frozen apply, captured provider reload, fallback sizing and native token counts.

Reproducible local evidence: `flash-bench/analysis-current-lcm/fork-trace-audit.md`
and `REPORT.md` in the usage-analysis workspace. These benchmark aggregates
describe this one run; they are not provider-wide reliability measurements.

At the published Go off-peak rates ($0.15/M uncached input, $0.003/M cached input,
$0.60/M output), the system/control call estimates $0.000814 versus $0.001805
for the cold bounded episode. Holding output length equal reduces the observed
advantage to approximately 21%. The 98% cache-input unit discount is not a 98%
discount on the entire summary. One cold 51k fork can outweigh several warm wins.

For any provider, compare:

```text
fork cost = uncached suffix * input rate + cached prefix * cache rate
            + summary output * output rate
bounded cost = new bounded input * input rate + its cached part * cache rate
               + summary output * output rate
```

Keep actual cache counts, output/reasoning, fallback/cold calls, latency and recall
quality together. Do not count reported reasoning twice, or convert token prices
into Claude/Codex subscription quota percentages without meter evidence. Do not
add a hidden heuristic cache-hit cutoff; start with an explicit selectable input
method and use measured results to choose its configured default.

Sources: [DeepSeek cache behavior](https://api-docs.deepseek.com/guides/kv_cache/)
and [Go rates/session routing](https://opencode.ai/v2/docs/console/go/).
Detailed local probe: `flash-bench/summary-fork/REPORT.md` in the usage-analysis
workspace. That artifact retains failures, body/prefix hashes and all ten calls.

## Implementation order and acceptance

1. Add wire-prefix regression coverage before changing rendering: normal append,
   system/control append, tool disable, schema repair, skill/memory refresh,
   live capability change, completed tool-loop transition and provider fallback.
   Verify ordered old blocks and frozen declaration/config fields, not just the
   high-level prompt string. Include Chat, Messages and Responses.
2. Separate immutable declarations from local admission. Use generated ordered
   controls for state changes and fix late-system hoisting on the dynamic path.
   Preserve existing native-thinking compatibility and real effect admission.
3. Introduce a small optional prepared-request capture and fork builder. Keep
   bounded episode input selectable. Test source-boundary mappings, no future
   leakage, no main-chat mutation, no tool execution, cancellation and deletion.
4. Run paired live episode-only versus fork continuations on actual BubblePaw
   requests, counting every call and cache miss. Repeat across source growth and
   at least one LCM apply, including native-thinking/stateful continuation where
   supported. Same provider/model/route, not mixed-model anecdotes.

Steps 1 and 2 retain broader protocol/declaration work. Step 3 is implemented
for Chat in r6, with focused wire/runtime coverage; step 4 remains a paired
cost experiment beyond the synthetic integrated recall check. The existing LCM
gather/apply split, durable host notices, frozen memory/skills and single JS entry
point should be retained rather than replaced by another orchestration system.
