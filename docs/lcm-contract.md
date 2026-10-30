# LCM context contract

Status: approved 2026-10-19, implemented 2026-10-19; local contract checks and a fresh synthetic live
automatic-pressure/post-apply qualification completed. This
contract supersedes the r6 claims of sufficient summary qualification; the r6
benchmark demonstrated malformed episode memory actually served to a worker.

## Three independent states

- Canonical messages and delivered tool results remain append-only and retrievable.
- Background gathering creates episode/chapter memory with exact source coverage.
- Apply publishes a frozen serving view for subsequent main requests. Gathering
  alone cannot change that view or an earlier request prefix.

## Chat prefix and tool catalog (2026-10-23)

The chat's initial tool declarations are persisted and reused across runs and
process restarts. Neither reload nor summary apply replaces them. The system
prompt is also persisted; apply may rebind its skill catalog and memory files
from current state (approved 2026-10-30), while ordinary turns and reloads keep
it fixed. The callable inventory stays current through `tool_search` and `call_tool`;
a new chat declares the core tools available when it starts.

At apply, rebuild a cold tool catalog with current deferred-server anchors and
changes since the chat started. Deferred additions carry names and server names;
core additions absent from the declarations carry their full definitions with
compact JSON schemas. Removals carry names only. Ordering is deterministic.
Updated definitions use the same core/deferred forms.
Persist this catalog until the next apply. Covered tool-change notices leave the
serving history with the rest of the summarized messages; canonical history stays
intact. The declarations remain byte-identical; the system prompt is stable
between compactions.

A chat also retains its initial `submit_result` declaration, or its absence.
A later run with a new or different schema announces the full current definition
in a host notice. Direct calls and `call_tool` validate against that run's schema;
a run without an output schema refuses submission with a normal tool error.
After the contract changes, each later run announces its current contract so a
return to an earlier schema does not leave a contradictory notice in history.
Apply refreshes a changed run-schema notice so its full definition remains
available after the earlier notice is summarized.

## Gather

Accumulate new canonical context, not repeatedly billed input or a count of model
rounds. At a complete exchange boundary near the configurable source target
(24k default, 32k planning maximum), gather an episode asynchronously for roots,
children and hidden workers during a continuous assignment.

Fork the compatible prepared main conversation and append the summary control.
Preserve the opening system, skills, tools, tool choice, replay-compatible state
and provider generation. Source selectors identify an exact historical range;
earlier history is background, not new events or summarizer instructions.

Native Chat parents that already declare `call_tool` retain their complete tool
declarations, including any main-task `submit_result`, byte-for-byte. An appended branch
control asks for `call_tool({name:"submit_summary",arguments:{summary:"text"}})`;
chapter forks use `submit_chapter` with the same `summary` string, including the
existing heading and final `Related:` evidence line. These targets exist only
inside fork dispatch, never in the main registry or discovery. The dispatcher
extracts exactly one matching return envelope and sends its text through the
ordinary summary/chapter validation and parsing. Any other declared tool, wrong target,
malformed payload or multiple calls gets an error result and one retry without
executing a tool; a second failure preserves coverage and reports
`tool_call_returned`. Undeclared tool names are refused by the provider adapter.
Plain text remains accepted, including for JS parents. Providers receive no
response schema: structured main-task results use the `submit_result` tool's
input schema. Forks refuse `submit_result` (directly, through `call_tool`, or
inside JS) without execution and use the same one-retry rule. The main run
continues to refuse the fork-local `submit_summary` and `submit_chapter` names.

Use the captured successful route, followed by its explicitly configured provider
fallbacks. Free first applies when the captured main route is still free; a
confirmed paid main route does not repeatedly restart a failed free route. Route
fallback retains the full-fork shape. Episode-only input is an explicit policy,
not a silent response to unavailable capture, unsupported protocol or pressure.
Different routes are not assumed to share provider caches.

Full-prefix forks currently require the Chat adapter. Messages/Responses callers
must explicitly select `context.lcm.summary_input = "episode"` until native fork
support exists. Archive-replay convenience constructors explicitly select episode
input because no live main request exists; their reports identify the input mode.
Replay's explicit `--threshold` remains an input threshold, unlike the live
runner's physical-cap setting.

Summary sizes are semantic token targets, not tiny combined reasoning/output
limits. Failed, empty or protocol/action-only output is not memory, nor is output
with tool-call markup (DSML, invoke/tool_call/function_call tags, `*** Begin
Patch`) or episode-transcript labels anywhere outside a fenced block or inline
code span. Preserve its
reported usage, retry/fallback under the configured policy and leave coverage
unchanged if no valid summary is available. No arbitrary minimum prose length.

The episode summary prompt is `summary_prompt_v2.txt` unless
`context.lcm.episode_prompt = "v3"` selects `summary_prompt_v3.txt` (2026-10-27,
revised as v3.1 on 2026-10-28; the first draft remains only as the bench-only
`v3-draft` value).
v3 keeps v2's rules and length limits and adds a line-2 gist and an `Actions:`
log before `Open:` for the compacted-history UI; see
[config](config.md#lcm-sizes-and-background-gathering) and the
[v3 comparison](lcm-episode-v3-comparison.md). Validation is the same for both;
a v3 summary missing its `Actions:` section is still accepted as memory.
Chapter synthesis defaults to `context.lcm.chapter_prompt = "v1"`. Opting into
`"v2"` adds two blocks after the prose: `Decisions:` followed by `- <decision>`
lines of at most 100 characters, or `Decisions: none`; and `Sections:` followed
by `- <name> | episodes <first>-<last>` lines (`episode N` is read as `episodes N-N`, and an en dash is accepted in the range). Names contain one to five words.
The episode numbers are the ordinals shown in the chapter input. Sections cover
every member episode in order, with no gaps or overlaps; even a one-episode
chapter has one section. Decisions must be grounded in the episodes, including
their decision and user correction Actions where available. Both fields are
plain copy for later display.

On a valid v2 response, the chapter's existing SQLite JSON `data` includes
`decisions` (an array of strings, empty for `none`) and `sections` (objects with
`name`, `first_episode` and `last_episode`). v1 chapters have
neither key. If either block is malformed or sections fail to cover the
chapter, the close still stores the description and summary and emits a parse
warning, without either new key.

An empty text reply (including reasoning-only output) gets one additional attempt
with a short host-generated user nudge to write the requested summary now; a second
empty reply still fails with `empty_output`, without advancing coverage. This
applies to the shared episode, chapter and classical summary paths, including LCM
forks, and retains usage from both calls and the existing retry/fallback policy.

Persist stable source message/tool-call IDs, never wire/prepared position numbers.
Positions quoted in prior summaries may refer to different rows after apply.
The appended branch control binds current IDs to current content, role and origin;
new changes are distinct from briefly labeled prior context. END is a snapshot,
not evidence that summarization interrupted the main task. Chapter inputs delimit
complete episode memory without adding conflicting numeric episode headings.

By default, the decider checks phase/subject continuity when an episode completes,
using accumulated open-chapter state. A transition closes the previous chapter
and starts the new one at a source boundary. Decider errors remain fatal. Set
`context.lcm.decider_enabled = false` to skip all decider calls; episode gathering
continues, with chapter closure only through a positive `chapter_rollover_tokens`.
Volume rollover defaults to zero (disabled); no fixed episode count closes chapters.

### Chapter references (2026-10-23)

Closed summaries start with a stable `[ch-N]` line, using the chapter's immutable
session ordinal. New summaries persist that line once; older summaries receive it
when a new serving view is applied. Later turns keep the frozen prefix unchanged.
`history_chapters` exposes chapter IDs and quote-supported relations. Existing
history forks continue to rebuild chapter metadata rather than copying it.

Side calls that share the chat's model fork its context (P6, see
[prefix-cache-design](prefix-cache-design.md#side-calls-as-forks-p6-2026-10-21)):
a summary fork sends the main prefix cut at the episode's last message; a
chapter close sends the latest confirmed main capture cut at or extended to the
chapter's last message with the chapter task in the tail. Neither changes the
main lineage or history. Episode-only summaries, deciders and any side call on
another model or other settings are fresh and declare why.

## Apply

Before the next main dispatch, compare projected input plus output reservation
with the effective physical context cap. A 100k cap with a 16k response reserve
permits 84k input; the reserve is deducted exactly once.

Root/child apply caps constrain main dispatches. A summary fork independently
checks its input plus response reserve against the smaller of the configured
context window and that candidate model's window. Its appended branch control
needs additional room; the main apply cap is not a second fork-window setting.
For example, a main 18k apply cap and configured 32,768-token model window permit
a fork above 18k while still requiring main inputs plus reserve to fit 18k.

Under pressure, wait for needed gathering and publish an admissible view with:

1. The unchanged opening and original initial task.
2. Steering anchors: the first and latest typed or ChildTask anchors covered by apply.
   They are exact original messages, deduplicated, in history order, at most four.
   They are served only when apply covers history.
3. The subject chapter index for historical navigation, bounded by `index_tokens`
   (default 2000). It lists closed chapters with covered episodes, newest first.
   Open chapters are never listed. Each description is excerpted to
   `chapter_description_tokens` (default 300). An omitted-row count follows when rows do not fit.
4. Complete closed-chapter summaries, without duplicated member episodes.
5. Partial closed-chapter episodes: when apply covers only some episodes of a closed
   chapter, those covered episodes are served in place of its summary. They appear whole,
   with no separate budget. Items 4 and 5 share one closed-chapter block, in chapter order.
6. Complete episodes of the current open chapter.
7. The exact uncovered raw suffix, including its existing generated notices.

Applied compaction may rebind the session's chat surface: the skill catalog and
memory-file prefix are rebuilt from current state, and covered change notices
leave the serving view. The new snapshot becomes the durable current surface;
the original remains available for history. Change detection restarts from the
new snapshot, and reopen replays only notices after that snapshot. Classical
summarization follows the same rule. This happens only at applied compaction;
there is no independent size-based rebuild trigger.

The view carries no runtime inventory. Between compactions the system prompt is
byte-identical; an apply may also rebind its skill and memory-file content as
described above. Tool declarations retain their existing ordering and behavior.
The apply declares its prefix change as `lcm_apply`. View-dependent instructions, such as the short
original-history lookup note, are cold notes: one Generated message at the end of
the cold tier, `<harness kind="history_lookup">…</harness>`, ordered by kind and
then insertion. The lookup note is derived from the view on each build; the tool
catalog is persisted separately at apply. Both retain their text between applies. The full runtime
facts reach the model once, as the `runtime_inventory` tail reminder sent with the
first request after the apply.

Views stored before 2026-10-21 (P4) may carry a `system_prompt`, a bare lookup
message and a `[host notice]` inventory in their prefix. They are served
with their stored message prefix and get no cold notes until the next apply
replaces them. Once the chat prefix is bound, the harness preserves its system
prompt even if a legacy view supplies another one.

Apply consumes ready complete coverage. It does not keep covered raw episodes
merely to refill available space, mask tool outputs, silently reduce memory to
titles, or advance through a pending tool exchange. Failure leaves the prior
view unchanged. Emergency short-episode gathering under real pressure is distinct
from normal source-target cadence and must be observable.

Example: opening 8k + three raw 24k episodes + 6k unfinished episode = 86k input.
At a 100k physical cap and 16k reserve, apply a ready 5k chapter covering E1/E2
and a ready 2k E3 summary. The next input is opening 8k + chapter 5k + E3 2k +
exact 6k raw suffix + lookup cold note, about 21.2k. The closed chapter also adds its index
row, within the 2k index bound, and any steering anchors not already in the opening.
Original messages remain stored.

Apply changes the cold tier after the preserved opening. Later requests append to
this frozen view; newly completed summaries do not keep rewriting it. Client
prefix equality and actual provider-reported cache usage are separate evidence.

### Final admission recovery (2026-10-23)

Automatic preparation uses the captured provider's window as well as the
configured physical cap. The final guard still checks the complete assembled
request, including the combined tail reminder and cold notes/tool catalog.
If that guard exceeds the token or byte budget, it returns typed budget pressure
with the additional overhead missing from the preparation estimate. Before any
dispatch, the harness retries preparation over fresh canonical history with that
space reserved. This is automatic pressure, not manual compaction: ready complete
coverage is applied, or the worker's existing session gate waits for pending
gathering and performs ordinary/emergency gathering as needed. The rebuilt final
request is validated again. Original messages and completed effects are retained;
recovery does not rerun tools.

Every recovery after the first must advance the serving frontier. An unreducible
view still reports the context/byte-budget error; a stalled view cannot create an
unbounded preparation loop. Cancellation, gather failures and gathering timeouts
retain their existing diagnostics. Failed final admission does not deliver or
acknowledge queued reminders or classify/log a dispatched request. The trace event
`context_budget_pressure` records the missing token/byte overhead and frontier;
ordinary and emergency gathers keep their existing trace events.

Response calibration records the complete input immediately before dispatch.
It no longer compares provider usage containing final notes/reminders against a
pre-reminder estimate, which could carry those additions as a false residual into
the next request.

The motivating `20261023-094952-jev-free` trace has a correct 44,000-token input
budget (60,000 physical cap minus 16,000 reserve). After the last apply at record
119, summary/decision work succeeds at records 144/146, but subsequent main inputs
keep appending to the same cold prefix. Record 165 fails final admission at
44,114 tokens without another pressure-gather event. The preparation estimate
omits final cold notes and can differ from the calibrated final count; the final
guard previously had no recovery path. Frozen system/tool bytes remain constant,
so inventory growth does not explain the failure. Trace metadata does not expose
the exact split between cold notes, reminder overhead and calibration residual.

## Verification

### Recorded compaction facts (2026-10-27)

The chat database records `episode_written`, `chapter_closed` and `applied`
events with exact source ranges, episode/summary IDs and token counts. Triggers
are `source_size`, `subject_change`, `pressure`, `emergency` and `manual`.
Explanations are bounded metadata, never copied message or summary content.
Automatic admission applies from both `PrepareMode::Request` and
`PrepareMode::Pressure` record `pressure`; explicit `Compact` records `manual`.
Reusing the frozen view records nothing.

`tokens_before` is optional: it is null when any covered originals lack stored
source counts, including inherited imported summaries and legacy checkpoints.
The apply event and its full covered range are still recorded, even when a view
only freezes inherited memory. Never load cold originals solely to count them,
and never substitute a fabricated count. Native episode gathers retain their
known source counts; `tokens_after` measures the summary text.

Applied events capture the admission estimate before and after publication,
including system instructions and tools. The applied view records its ordered
block token partition; per-message reads distinguish provider-reported assistant
output tokens from read-time O200k estimates. Existing stores have no backfilled
events and their older view rows remain readable without block metadata.
Core reads expose reported counts only. The SDK combines them with estimates
from the runner's existing estimator; core has no tokenizer dependency.

Gather events share the episode batch transaction. Applied events share the
existing chat validation transaction for the view, including central layouts
whose disposable view lives in `serving.db`. Message usage links share existing
assistant writes. Ordinary turns reusing a view add zero transactions. Detailed
field meanings and trigger rules are in [context compaction](context-compaction.md).
Each fact also emits `context_compaction` on the session event stream. Its
notification shares the single-file transaction or the central layout's existing
buffered serving flush; it adds no independent commit. The stdio `context`
feature and matching HTTP reads are described in [Session API](session-api.md).

Required behavior checks cover request-prefix preservation, independent background
completion, exact source coverage and raw suffix, no memory-body omission,
invalid-output retries/fallback with usage preservation, failure without view or
coverage mutation, physical budget accounting and all worker kinds. Local mocks,
fresh live provider checks and task benchmark acceptance are reported separately.
Detailed operation tracing must be enabled for future LCM benchmark runs.

The 2026-10-23 scripted budget regression uses the real harness, LCM and gated
worker with a 60,000-token physical cap and 16,000-token output reserve. Legal
tool-result growth passes ordinary preparation, then overflows final admission
with a served cold note. Both not-started and pending gathering recover with an
applied summary, one tool execution, preserved canonical evidence and unchanged
frozen prompt/tools. Separate cases cover cancellation, oversized fixed overhead,
an unreducible original task, a host making no frontier progress, and calibration
against the dispatched reminder. Disabling only final-admission retry in a
temporary Linux source copy reproduces `48732 > 44000`; the source is restored
after that negative control. These are scripted/local-transport checks, not a
new live-model qualification of the delegated Flash task.

Final Linux checks for this repair pass one command/test at a time: workspace
formatting, strict all-target/all-feature Clippy (`-D warnings`), and 134 targeted
behavior tests (11 core host/fork tests and 123 runner budget, growth, gather,
model, manual-apply, calibration and local HTTP/fork tests).

The initial implementation passed 352 workspace library tests and 58 independently
rerun fork/worker/apply integrations. After narrow prompt/rendering follow-ups,
41 episode-model/native-fork integrations, strict all-target/all-feature Clippy,
formatting and build passed again. These are separate, overlapping test sets.

The installed archived executable is SHA-256
`e823cec91260a326b94a4d39251e78f6ebb00ae64ba2ba0ec2c435be11aac0aa`.
The final BubblePaw-only synthetic run demonstrated complete closed/open memory,
the exact delivered raw suffix, automatic apply, unchanged canonical history and
gather/checkpoint prefix, correct no-tool recall, and a completed fork from an
already applied prefix with correct source roles. Earlier failed/partial cases
remain recorded. See the supervising task's `flash-bench/lcm-contract-r7/REPORT.md`.

The first gather ran before the new tool exchange reached a prior main request:
its opening was already observed, but the new call/result were reconstructed and
only later corroborated against main wire bytes. No universal prior-cache claim
follows. Existing invalid memories are not migrated, minor prose errors remain
possible, and this fixture does not establish production 100k/200k throughput or
general task/summary quality. No native-harness benchmark was run.
