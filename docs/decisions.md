# V0 decisions

Status: decision log, dated; current through 2026-10-22.

## Native by default; JavaScript wraps native calls (2026-10-22)

New chats default to native tools. JavaScript is an explicit `execution_mode =
"js"` alternative; `enable_js` is removed. Chats retain their initial mode on
resume, and legacy chats without mode metadata retain JS. This supersedes the
2026-10-20 decision to retain the JS default and offer both surfaces together.

Both modes share validation, approvals, budget and durable effect boundaries.
Tool declarations and the frozen system prompt are fixed when a chat starts and
reused by every later run of that chat (2026-10-23). Tools added later, MCP
included, stay deferred behind `tool_search` and `call_tool`; an applied
summary refreshes only the cold prompt's tool catalog. Call independent tools together in one message;
the loop delivers the whole batch before requesting the next model response.

Structured results go through a `submit_result` tool, never a provider response
schema (2026-10-23). Summary and chapter forks resend the parent request
byte-identically and deliver memory via `call_tool` to branch-only
`submit_summary` or `submit_chapter` targets; these names remain unknown in the
main run, and forks refuse `submit_result`. See [execution modes](codemode.md)
and the [LCM contract](lcm-contract.md).

## One central store, first process hosts (2026-10-21)

Renamed 2026-10-24: the folder is now `BubblePaw-Core`, and the old `BubblePaw-base` store is imported once as a legacy source. See [storage model](storage-model.md).

Each machine gets one central BubblePaw store per user, by default
`%LOCALAPPDATA%\BubblePaw-base\` (XDG data home on Linux), instead of one
store per workspace path hash. The per-workspace default was never a decision;
it followed from the store-wide workspace binding. Each root chat now owns its
workspace and carries its own metadata (workspace, project, origin, parent).
`--data-dir` stays for isolated tests and benches.

The first process to take the store lock is the host. Later `run` and
`delegate` processes attach to it over a current-user-only local endpoint
published in `host.json`, and their work runs in the host as ordinary sessions
with their own configuration and workspace. There is no daemon: the host exits
once its own work and attached clients finish. A client refuses a host whose
binary or protocol version differs. Shared state (quota, usage, web cache,
delegation ledger) is written by the host. Existing per-workspace stores are
imported once into the central store; their directories are only read. A
process that runs its own session and registers it with the host is deferred
until host crashes prove a problem. Status and progress: [host attach](host-attach.md).

## Hybrid execution and embedder tool APIs (2026-10-20; mode policy superseded 2026-10-22)

Historical decision: the JS default and additive `enable_js` policy below are
superseded by the 2026-10-22 decision. The shared embedder API remains supported.

JavaScript is no longer the main execution-mode direction. Add native execution
as an opt-in: registered Rust capabilities become top-level model tools using
their existing implementations and schemas. JavaScript can also be enabled
alongside native tools. Keep the current JavaScript default unchanged for
existing hosts, ongoing work and benchmarks; a later default change belongs to
the user. This supersedes the sole-JS-entry-point decision below.

Configure `[agent] execution_mode = "native"` and optionally `enable_js = true`.
The default remains `execution_mode = "js"`. The library exposes effective tool
definitions and a model-free single-tool call on `Harness`. Both use the same
session restrictions, dispatch, approval policy and durable effect boundaries
as ordinary model calls. Calls without an interactive client refuse an approval
that would require a person, including a nested call inside JavaScript. Unknown
effect outcomes are never replayed. See [execution modes and embedder APIs](codemode.md).

## Literal edit and whole-file write (2026-10-20)

The workspace surface is `read`, `list`, `search`, `edit` and `write`, registered
host tools exposed directly in native mode or through `js` when enabled.
Literal `edit` takes `path`, `old_string`, `new_string` and
optional `replace_all`, returning `{path, replacements}`; whole-file `write`
takes `path` and `content`, returning `{path, bytes, created}`. No prior read or
hash is required. Keep staged atomic replacement, write-time comparison,
approvals, child workspace rebinding and durable checkpoints. The 32 MiB file
limit and 10 MiB checkpoint barrier remain; compact results carry no checkpoint
field. This supersedes the dated patch-format decision below.
See [file tools](file-tools.md) for exact matching and recovery diagnostics.

## P3 session metadata, usage and exec floor (2026-10-15)

The settled P3 features reuse existing storage boundaries. Titles stay in
session metadata and its catalog projection; rename commands add no prompt or
model call. Fresh CLI runs can use a temporary store selected before migration
or opening the configured store. Archive retention is opt-in, startup-only and
uses actual archive transition times in existing metadata. Unknown legacy
archive dates are retained. Whole-chat deletion owns resource cleanup.

Usage is a runner projection of retained records, with own and descendant
totals kept distinct. A context adapter records one bounded latest-request
observation through existing host state. A small optional response metadata
field separates the final physical HTTP request from cumulative continuation
usage, including an explicitly unknown final count. This costs one transaction per
completed response. Unknown counts, capacity and pricing stay unknown; hosts
may supply exact model tariffs, and costs are explicitly partial. Original
history is unchanged, and reporting does not restore archives or call providers.

The fixed exec blocklist lives in `approvals/blocklist.rs`, before configured
denies and every approval mode, including the direct-call optimization. The
default-on `approval.hardline` switch is live and inherited by children. It is
command-text insurance, not a sandbox. No model-facing tools are added.

Workspace matching uses existing bound-workspace metadata plus a small optional
engine resolver. The runner resolves each session's worktree; the matcher protects
that root and the parent workspace while resolving targets against exec's cwd.
The engine gains no child-worktree policy or additional persistent state.

Core cancellation signals its run token before invoking host cancellation hooks,
and releases the admission guard before those hooks run. A hook that releases a
blocked provider or JavaScript call therefore cannot let that active run commit
as completed before cancellation reaches it. Client attribution and background
and compaction cancellation are retained.

Local llama-server uses the existing chat kind. Only loopback chat endpoints
may omit key configuration; requests then omit Authorization. There is no new
provider kind. Acceptance is scripted and local-fixture only.

## Rewind and control state integration (2026-10-15)

Merging control main `c86f6fe` into rewind preserves approval modes, deny rules,
MCP filters, todo/question routing and all checkpoint entry points. Rewind forks
todo state at the selected canonical message and never copies a pending question.
The original session keeps its current list and question.

Latest-only todo storage cannot reconstruct earlier state when JavaScript discards
inner tool results. Todo therefore opts into generic host-state snapshots using
existing immutable attachments and content-addressed blobs. Latest row, snapshot
and event share the existing single update transaction. Question state remains
latest-only. Snapshot boundaries are remapped into forks for subsequent rewind;
legacy unsnapshotted values default to an empty list rather than guessing history.
No additional SQL schema is needed. This supersedes the control-only latest-value
storage description for todo; the rest of that decision remains in force.

## File checkpoints and rewind (2026-10-15)

Built checkpoints for the former patch tool and the user rewind path. Immutable
attachment rows reuse the existing compressed content-addressed blobs; no SQL
schema change is needed. Records are durable before filesystem effects and carry
workspace, session, tool invocation, canonical message boundary and patch order.
At that date, files over 10 MiB received explicit result skips and durable
rewind barriers; the current compact edit/write results omit those fields.
Patches have a separate bounded 32 MiB file limit; ordinary reads stay at 8 MiB.

Rewind forks canonical history through a selected message and restores only the
source session's later patches. It compares current content with the last
post-image, checks adjacent checkpoint continuity, and skips conflicts without a
force option. Source history remains intact, archives preserve checkpoints, and
deletion prunes them. Exec and outside changes are not checkpointed. The runner
owns file policy; core gains only an inclusive fork cutoff and a generic callback
that keeps admission excluded through host restoration. No new model tool.
See [rewind](rewind.md) and [verification](verification.md).

## Control/context/children integration (2026-10-15)

Merge `main` at `e9cfb1f` into the `control` branch, retaining manual compaction,
skills, output spilling, recursive child scopes and optional worktree bindings
alongside deny rules, MCP filters, todo and questions. Stdio advertises all three
new capabilities (`compact`, `todo`, `question`); configuration recognizes both
skills and approval denies. Child continuation keeps its workspace runtime,
remaining parent controls and recovered-answer handling.

Question notices use the existing required direct-parent wake path. The children
this change registers that path for hidden worker parents too, so their ordinary
children can wake them to answer through `agent_send`, while the worker remains
silent toward its visible caller. This supersedes the original control-only
hidden-parent limitation without adding a second wake lifecycle. Scripted
CodeMode fixtures prove direct-parent grandchild routing and hidden-parent wakes
with optional completion wakes disabled. Deny dispatch fixtures verify additive
inheritance from root through three descendants, including pinned overrides and
profile-specific rules. See [verification](verification.md).

## Control tools and policy (2026-10-15)

Approvals are split into `approvals/{mod,modes,rules}.rs`; auto approval stays
as built, with further work deferred. Configured denies use the actual existing
session-rule grammar: exact tool names, without argument predicates or globs.
Denies are checked before session allowances and every mode, including pinned
run overrides; child policies union their parent's denies. `[approval]` is
accepted as an alias of existing `[approvals]`, with conflicting same-file
tables rejected.

MCP filters use raw server tool names and `*` globs. `include_tools` is optional:
unset means unrestricted, explicit `[]` hides every tool. Excludes win. Legacy
`include`/`exclude` remain compatible. Filter-only changes reuse the connection
and cached unfiltered inventory; registration and notified refresh apply the
same filter path. Replacing connection settings keeps generation draining.

`todo` replaces one bounded latest list per session; `question` keeps one pending
question per session. Both remain host capabilities reached through `js`.
The core gains only a generic bounded durable host-state table and change event;
runner modules own all tool semantics. Accepted questions/answers survive a
restart; recovery admits generated answer input and never reconstructs or
replays an interrupted JavaScript cell. Retried recovered answers are deduplicated
against accepted generated input. Child questions route to the direct parent
and wake it even when optional completion auto-wake is disabled; explicit Stop
suppression still applies. `agent_send` answers the existing child wait.

Root questions use the stdio session API or the workbench's simple answer form.
Unattended `run`, `resume` and `script` return `no_user_available` immediately.
Todo lists appear in snapshots/events and the workbench, without prompt injection.
Live question answers are accepted as generated input before releasing the
waiter, so sequential questions in one JS cell cannot overwrite an undelivered
answer. The existing input queue and canonical marker provide the receipt.
Script-only runs consume generated steering after their tool result without
starting a model; explicit user followups keep their existing behavior.
See [session API](session-api.md), [approvals](approvals.md), [MCP](mcp.md),
[child sessions](child-sessions.md) and [verification](verification.md).

## Children/context integration (2026-10-15)

Merge `main` at `961038e` into the `children` branch, preserving both feature sets.
Workspace-bound file adapters share the host's session-keyed external skill read
grants while retaining separate write locks per workspace. Skill discovery uses
the session's effective worktree, so committed child instructions are distinct
from dirty parent files. Process views retain the host spill store as well as
shared job limits and lifetime. Spill cleanup is attached before early CLI
commands; archive/delete continue to apply the worktree lifecycle rules.

## Recursive children and optional git workspaces (2026-10-15)

Recursive admission remains uncapped by default. The catalog parent chain owns
the tree; status lists descendants with direct parent ids, while send/status
addresses retain direct-parent ownership. Existing optional caps and approval
clamps remain. Spawn and explicit continuation inherit the originating tool
dispatcher's remaining call budget and the active parent's remaining deadline.
Automatic completion turns retain their ordinary fresh-turn defaults. Internal
workers remain silent outward; ordinary hidden descendants wake their direct
parent, including a hidden worker. Per-run worker completion receipts permit
those automatic turns without allowing stale acknowledgement to delete them.

`worktree: true` is an opt-in spawn argument, default false. Runner adapters bind
file tools, commands, JS captures and memory discovery to the child checkout.
The branch is `bp/<child-session-id>` from the direct parent's current HEAD;
parent dirty changes are excluded. The BubblePaw data directory must sit outside
the source repository. No new model-facing tool or store schema is introduced.

Archive/delete cleanup follows successful SQLite lifecycle changes. A small
lifecycle mirror beside worktrees lets cleanup inspect inactive identities
without reopening archived chats. Dirty work, unmerged commits and worktrees
needed by retained descendants stay. Only a worktree deliberately removed by
cleanup may be recreated on explicit continuation; unexpected missing paths
require repair. Library tool adapters use dispatcher-aware spawn/send entry
points to preserve exact remaining budgets; context-only APIs retain explicit
host budget allocation.

## Manual context controls, skills and output spills (2026-10-15)

Manual `/compact [focus]` and `session.compact` use the existing LCM path
immediately, bypassing its automatic trigger. Optional focus text is an extra
summary instruction for that operation. There is no model-facing compaction
tool, and original messages remain unchanged.

Skills are ordinary folders containing `SKILL.md` with YAML `name` and
`description`. Discovery prefers the workspace's `.agents/skills` and
`.claude/skills`, then those of its parents through the Git root, then
`~/.config/bubblepaw/skills`, then configured `[skills].dirs`. The first name
wins. Skills are enabled by default. A session freezes a bounded list of names,
descriptions and absolute entry paths; the model reads bodies through ordinary
`read`. Turn-start stat checks send a durable name diff once when skills change,
without rewriting the frozen system prefix. External read access covers listed
skill folders and the current chat's own spill files only.

The catalog budget is 8 KiB. Even an empty or disabled initial catalog gets a
small frozen attachment, so enabling skills later cannot change an existing
session's system prefix. Skill bodies are hashed for change detection but are
not copied into SQLite. `[skills].enabled` and `dirs` apply at turn start; newly
announced folders become readable while previously listed folders retain their
read grant. Unchanged checks add no application transactions.

Tool output over 64 KiB is preserved under the BubblePaw data directory as
`spill/<chat-id>/<id>.txt`; the model receives head and tail excerpts, the path
and byte size. JavaScript returns use the same store. Spill contents stay out of
SQLite, have a bounded per-chat disk budget with oldest-first eviction, and are
removed with their chat. Archiving retains them. These files are disposable
output retention, not a second conversation authority or a new tool surface.

The spill budget is 256 MiB per chat. Existing process capture bounds remain:
complete captured output spills at terminal observation, while capture loss is
reported explicitly instead of labeling an incomplete file as full output.
JavaScript's serialized-return ceiling rises from 1 MiB to 8 MiB under its
existing 32 MiB heap limit; the model's bounded text reply stays at 60 KiB.
Line-range spill reads scan bounded buffers, including files larger than the
ordinary 8 MiB workspace read limit. Filesystem cleanup uses a small generic
store lifecycle hook, installed before early CLI deletion paths as well as in
long-lived hosts. Store schema stays unchanged.

## Repo sub-agent essentials (2026-10-15)

At this date the model-facing file tools were `read`, `list`, `search` and the
former patch tool. That tool used the Codex patch format for adding, updating,
deleting and moving files, replacing the earlier `edit` and `write`. Context
matching was exact, without fuzzy or whitespace-tolerant fallback. A hunk needed
exactly one match; errors named its file and hunk. Entire patches validated before
writing, retaining canonical workspace paths, approvals and write-time comparison.
No read hash or prior read was required. That superseded the earlier same-day
batched find-and-replace decision; the 2026-10-20 decision above supersedes it.

The runner loads user-level `AGENTS.md`, then `AGENTS.md` and `CLAUDE.md` from the
git root through the workspace, outer first. Without git, discovery starts at the
workspace. Identical contents load once; imports are not expanded. The bounded
32 KiB snapshot stays frozen for the session. Turn-start stat checks deliver
changed, added or removed files once through durable host notices, preserving the
system prefix. Children take their own workspace snapshot. The default-on feature
can be disabled with `[context] memory_files = false`.

`read` supports PNG, JPEG, GIF and WebP as model input, including inside `js`.
Images have count and byte caps, repeated reads deduplicate, and oversized images
produce a text note without resizing. Provider adapters use their supported image
forms or give a short unsupported-input note. Content-addressed blobs store each
image once; history keeps references and replay reads the stored bytes. Reasoning,
continuation and unknown-effect replay rules are unchanged.

See [file tools](file-tools.md), [memory files](memory-files.md),
[providers](providers.md) and [verification](verification.md).

## Dual-era Streamable HTTP MCP client (2026-10-14)

MCP tools support Streamable HTTP beside child stdio. The HTTP client primarily
speaks 2026-07-28, carrying protocol and capability metadata on each request;
there is no modern session, GET stream, or DELETE. A read-only modern tools/list
probe follows the spec's error-body-based era detection before falling back to
initialize for 2025-06-18 or 2025-11-25 servers. Recognized modern errors are not
mistaken for older servers. An unsupported-version error may explicitly select
an advertised compatible revision; incompatible lists fail. Stdio retains its
existing handshake.

Earlier HTTP sessions round-trip Mcp-Session-Id and the negotiated protocol
header, end with DELETE on explicit shutdown, and resume interrupted SSE
responses through GET with Last-Event-ID where supported. A confirmed stale
session rejection permits one reinitialize. A call with an ambiguous external
outcome is reported unknown and never replayed; retrieving response events
does not resend the tool call.

HTTP endpoints require HTTPS except loopback hosts. Authentication headers name
environment variables only, using the web-provider suffix rule or the explicit
[mcp] header_key_envs allowlist. Redirects, automatic retries, and inherited
proxies are disabled. OAuth and deprecated 2024-11-05 HTTP+SSE remain outside
scope. BubblePaw is client-only; core and the authoritative store are unchanged.
See [MCP](mcp.md) for the contract and local verification.

## Completion wakes idle parents (2026-10-14)

Idle parents are woken by visible child and background-job completion, on by
default; workers never wake. Every terminal child result qualifies, including
timeout, budget-exhausted final answer, error and cancellation. Background exec
jobs qualify only after backgrounding. Nearby completions share one generated
`[host notice]` input and one parent turn, using a configurable short debounce.

Busy parents keep the ordinary next-model-request notice path. Explicitly stopped
sessions stay suppressed until accepted user text; archived, closed and hidden
sessions are ineligible. Wake chains have no cap. Scheduling and completion
ownership belong to the runner; core exposes only generic admission and notice
hooks. Web and stdio forward wake-turn events through their normal streams.
Restart never schedules old completions automatically or replays unknown effects;
pending completion information is delivered on the next explicit turn.

## Invisible background workers and transient chats (2026-10-14)

Internal host jobs use the normal child-session path, marked hidden. User-facing
sub-agents remain visible. There is one model/tool runtime; the runner does not
assemble its own inner loops. Worker profiles choose tools and prompts while
ordinary child controls govern schema, tool budget, timeout, approval and effects.

After a successful result is delivered or persisted, the caller explicitly
acknowledges it and the worker session is deleted. Failed, cancelled and timed-out
workers remain inspectable until retention cleanup. `agents.worker_retention_days`
defaults to seven days; `agents.worker_keep_all` preserves all worker chats for
debugging. Hidden descendants stay hidden. Running sessions and unresolved effect
boundaries prevent cleanup; an unknown external outcome is never replayed.
Workers accept one job. Delivery acknowledgements bind to its run receipt;
visible children retain their ordinary follow-up turns.

`run_jobs` keeps its public name and runs concurrent workers in input-result order,
with optional caller-selected concurrency and independent per-job controls. There
is no default worker concurrency cap. 

One tools-disabled model request can stay direct. Summary validation retries of
the identical request, model fallback, transport retries, truncation continuations and decision sampling do
not execute tools and remain at their existing boundaries. Chat titles currently
come from literal caller/task text and require no model worker.

The broader context proposal is recorded in
retrievable session compaction (an earlier proposal, kept in the project notes). Its approved first layer
is specified in the earlier context chapters design, kept in the project notes.

## Readable automatic summarization (2026-10-06)

The single rolling checkpoint described below was superseded for new compactions
on 2026-10-06 by context chapters (an earlier design, kept in the project notes). Existing checkpoints
remain frozen legacy summaries; new compactions append immutable chapters.

BubblePaw owns readable checkpoints instead of relying on provider-native opaque
compaction. The runner triggers at the smaller of 256,000 tokens and known model
capacity minus a fixed 16,000-token reserve. This replaces the initial 80% rule.
The library retains its full-history default and a small async
context-preparation hook; the summary policy and runtime facts belong to the host.
Local token estimates include instructions, tool definitions and active messages,
calibrated from per-request usage rather than summed billing totals. Unknown capacity
is explicit and can be supplied by the runner flag.

An internal tools-disabled call uses the selected model and returns a bounded
Markdown checkpoint. SQLite schema v3 saves one current checkpoint and its exact
canonical cutoff per session. Original messages and effects remain untouched;
forked history starts without a checkpoint tied to the source's message IDs.
The model sees the checkpoint, recent complete tool rounds, the literal latest
user request and a newly generated runtime note. A completed oversized tool round
can itself be summarized during a long task. An oversized literal latest request
fails clearly rather than silently discarding user instructions.

Runtime notes contain only current REPL generation/status/key names and owned
command/agent handle status. They are bounded and transient. Summary timeout,
cancellation, invalid output or an unreduced context budget does not replace the
previous checkpoint. This describes the original rolling checkpoint policy.

The goal is a usable, extensible BubblePaw base with a small Pi-style agent loop.
Build the harness first. Advanced context strategies and an experimentation
framework are outside this slice.

## Ownership

The Rust library is primary. A thin command-line/process runner consumes its
public API. The first local web host embeds it directly. The optional stdio
runner remains available for hosts that need process isolation/reloads. Providers and native
workspace tools live in the runner adapter layer, not in the engine. Host-defined
tools use the same Tool interface.

The core owns coherent model/tool history, queued input, cancellation, and the
minimum durable boundaries needed for restart. Outer model tool calls execute sequentially;
CodeMode cells can compose concurrent native calls through the shared dispatcher.
Steering applies after the current model/tool round. Cancellation explicitly
interrupts current work. Unknown external effects are never replayed automatically.

## SQLite authority

SQLite stores canonical conversation and session/execution state. Completed
messages and necessary external-effect checkpoints are durable. Provider token
fragments, displayed paragraphs, process-output chunks and other progress are
transient. Group related state changes into transactions. Bound stored tool
results. Observe file size, WAL size and transaction count in acceptance checks.

## Serving projection

BubblePaw owns a disposable in-memory serving database rebuilt incrementally from
authoritative history. It is for presentation, not execution decisions. Its schema
can evolve independently. Persistent serving storage can follow a demonstrated
startup or indexing need. The optional web host now implements this projection;
the core exposes bounded canonical reads and does not depend on the serving
schema. Paragraphs stay in bounded RAM, not either SQLite database.

## Scope

Include a generic Provider/Tool API, ordinary context preparation with original
history retained, paragraph/action events, SQLite session resume, steering,
follow-ups, cancellation, CLI and a minimal controllable stdio runner. Use an
explicit scripted provider for deterministic local acceptance and a Responses
adapter for real model access.

The first web workbench adds sessions, chat, actions and existing run controls,
using an optional HTTP/SSE transport. Child-agent orchestration, MCP, a
package/plugin platform, experimental memory/context systems and migration of the
old database are deferred. Existing BubblePaw repositories remain intact.
The MCP deferral is lifted for client tools only (2026-10-13): see [mcp](mcp.md).
Resources, prompts, sampling and elicitation stay deferred.
Approved 2026-10-19: BubblePaw may be served as an MCP server for an agent
gateway. Web chat apps get the `js` tool (plus `environments`) through one
hosted OAuth URL; what is available inside `js` follows the host agent's settings.
Approved 2026-10-14: live config refresh, including MCP servers, approval,
agents and exec; see [live-config](live-config.md). Hooks and plugins stay deferred.

## Acceptance

Prove an actual tool task, a second turn using stored history, process restart and
resume, cancellation during work, custom host tools, and bounded storage writes.
Separate scripted/local transport evidence from live model evidence. Do not call
this a finished BubblePaw migration or a stable cross-provider API.

## CodeMode v0

The runner embeds QuickJS; the Rust core has no JavaScript engine dependency.
Named per-session environments retain only explicit `state` between fresh async
cells. Rust capabilities use the same schema validation,
cancellation and effect boundaries when invoked through `tools.*`.

Explicit waits can outlast ordinary call deadlines. User steering backgrounds a
waiting cell and restores model control; another environment can investigate.
Wake resolves time waits early, while cancellation stops owned work. Failed or
cancelled cells reset their heap and increment generation in v0. Heap snapshots,
automatic replay and a package/plugin loader remain deferred.

Only explicit text/file/image outputs are newly archived; process progress and heap
inspection stay transient and bounded. Captured bytes live beside SQLite.
See [CodeMode API and limits](codemode.md).

## Experimental command templates and behavior tracing

The optional JS `cmd` tag preserves literal pieces and value boundaries. Rust
validates a deliberately narrow single-program grammar and launches native argv
directly. Shell-style rendering is only for display; raw command strings retain
multiline shell behavior. This avoids PowerShell 5.1 native forwarding losses.
Shell grammar in tags and argument spreading are deferred.

Behavior tracing is optional, disabled by default and separate from both SQLite
databases. A small core observer supplies correlated model/tool/output metadata;
the runner owns bounded queued JSONL writing, rotation and offline reports.
Bounded content capture requires a separate opt-in. Records have configuration
labels and a scoped compiled-source fingerprint for later comparisons. Losses
and writer errors are explicit. No experiment framework or dashboard is added.
See [tracing modes and limits](tracing.md).

## Retired table trial

The built-in `ui.table` helper tested direct structured answers from a CodeMode
cell. It accepts named columns and rectangular primitive rows, validates them in
Rust, and archives the full table once. The model-facing result and environment
preview contain only the persisted output ID, title and dimensions. The frontend
renders the saved table with its originating turn and restores it after reload.

Limits are 16 columns, 200 rows and 32 KiB of serialized table data, within the
existing output storage contract. Invalid or oversized tables fail explicitly;
there is no silent row truncation. Rendering treats values as text. Publishing
does not finish the model turn. Custom renderers, model-authored HTML, plugins,
automatic approval and a special turn-ending signal are deferred.

Ordinary shell commands remain the default. Prompt guidance asks for concise
CLI output and warns against parsing incomplete captures. Process replies now
insert visible omission markers alongside their original byte-gap metadata.

The user reset scope to a basic web wrapper after an ordinary full-tree request
hit the process-capture bound and received a vague model-generated error. The
table helper and its prompt guidance are removed. Archived tables remain readable,
but no new table-publication API or custom answer platform is included.
The frontend keeps saved chats, messages, tool disclosures and basic run controls.
Environment management, frontend fork and theme controls are removed. Backend
session/environment capabilities remain available through existing APIs.
Command failures and incomplete captures retain concrete bounded diagnostics so
the model can correct them instead of treating output size as a filesystem failure.

## JS is the sole model entry point (superseded 2026-10-20)

The hybrid-execution decision above replaces the following tool-surface choice;
the recorded JS execution and durability semantics remain applicable in JS mode.

The runner always advertises one model tool, `js`. Native capabilities are
registered separately for calls inside JavaScript. Direct provider calls to
native names are rejected before starting an effect and return a correctable
error. Legacy `workspace_*` and `process_*` names, hidden-name bookkeeping and
alias selection are removed. The native schemas, effect boundaries and transient
action events remain necessary behind JS; reads do not become additional model
tool messages. Existing saved history remains readable without migration.

The JS tool description provides the API reference. Environment inspection and
controls are available through `tools.environments`, preserving background
diagnostics with one outer tool. A cell cannot reset, close or cancel itself.
Host cancellation and environment APIs remain independent. The generic Rust core
does not depend on JavaScript; the runner owns this one-entry-point decision.

The JS call returns ordinary result data to the model. A single expression returns
its value; multi-statement cells select data with explicit `return`. `text()` is
deliberate user-facing publication. Returning observations must not publish raw
tool dumps or reduce the observation to a publication preview. There is no
separate inspection tool. Undefined/no-return cells retain the publication/status
receipt. Results are bounded; oversized or unsupported values fail explicitly.

State changes are visible in the model's tool result after a compact `[---]`
separator. The notice reports keys, not duplicated values; unchanged cells retain
their ordinary JSON result. `state.reset()` clears saved state data within a cell,
while full environment reset remains separate. The runner encourages optimistic
batching of independent, likely-needed reads/checks in one JS cell, preferring a
few useful extra calls to more model round trips. Dependent steps and mutations
remain ordered; the runtime does not force unrelated operations into one batch.

The system prompt describes the task workflow and batching preference. API
arguments, result/status semantics, state lifecycle and limits belong to the
generated JS tool reference. Avoid repeating the reference or accumulating old
failure-specific instructions in the system prompt.

## One-shot subagents above the harness

Child orchestration belongs to the Rust host, which creates a fresh canonical
session and reuses the configured provider and existing harness. The generic core
has no subagent concept. Child profiles inherit the configured execution mode.
In opt-in JS mode, `tools.agent` starts work; native mode exposes `agent`
directly. Shared waits and cancellation route owned agent or process handles.

Explicit task/context transfer preserves separate conversations and JS state.
Children select a registered agent profile, defaulting to `read-only` workspace
reads. The `workspace` profile enables commands/writes when host policy permits;
the deprecated `access` alias still maps to these names. A profile owns its
harness, optional model override and round limit. The library builds child
harnesses so embedders use the same mechanism as the CLI. Built-in children
cannot spawn another child.
Two active children per parent and four per host bound the first version.
No process isolation or worktree isolation is implied by separate sessions.

One-shot children release commands and JS heaps before returning their final
answer/error/usage. Parent steering backgrounds its wait; parent stop cancels
owned children. Their outputs stay in their saved session until the parent
chooses to publish an answer. Spawn effects settle after canonical run completion
and cleanup; uncertain cleanup stays unresolved. Restart preserves history but
expires volatile handles. No orchestration UI or plugin framework is added.

## Native web research (2026-10-09)

The tool exposure, research modes and defaults in this entry are superseded by
the 2026-10-13 decision below. Address safety still applies.

Web capabilities belong to the runner library's `net` module, separate from the
optional `web` workbench. CLI web access is opt-in with `--web`. The default web
configuration exposes `research` and `web_fetch`; raw `web_search` is opt-in.
Research uses an ordinary profile with only search, fetch and JS, configurable
quick/thorough budgets and an optional provider/model override. Its synchronous
tool waits for the child and returns an answer with best-effort parsed sources.
Existing child concurrency, input/result and cancellation bounds still apply.

Fetch restricts schemes and content types, bounds time/body size and rechecks
each redirect. Default DNS/address checks refuse internal destinations and pin
the approved addresses for the request. The configured SearXNG endpoint is exempt
for search requests; it does not exempt arbitrary page fetches. Allowing private
addresses requires explicit library configuration. Search keys remain secret.
See [web tools](web.md) for the API and operational limits.

## Research children and shared web retrieval (2026-10-13)

No hand-built inner model loops: model-driven work runs through the normal
sub-agent path. User-facing sub-agents (like research) are visible children;
internal host jobs (chat titles, LCM/summarization) will run as invisible
background workers with transient chats on the same path.

Research is an ordinary profile started, queried and continued through agents.
It receives web search/fetch and file reads, and chooses its own retrieval plan.
The main agent uses the same tools directly for quick lookups. Remove the old
`research` tool and its configuration instead of maintaining a parallel path.
The inspected fact-checker consumes low-level source registration and the separate
parallel loop, so tool removal needs no deprecated consumer shim.

Use persisted short source IDs shared by a root chat and its children. Store the
authoritative ID-to-URL/title mappings as immutable root-chat attachments using
existing Store APIs; the engine needs no new schema or web concept. Models cite
IDs and numbered passages, never retyped URLs. Fetch converts HTML to readable
text and stores a stable passage split; optional query retrieval uses BM25 in code.

Keep backend order and fallback. Default to a 24-hour TTL and unlimited search/
fetch budgets; configured budgets count misses only. Shared responses live in
disposable `web-cache.db` beside the catalog, with LRU eviction controlled by
`web.cache_max_bytes` (64 MiB by default), separately from chat records. Deleting
that file loses no authoritative history or source mapping.

This change implements research and retrieval only. Migrating parallel jobs,
probes, chat titles and summarization to background workers is separate work.

## Model previews separate from JS data

Each model-facing JS result defaults to 10,000 O200k-estimated tokens, including
execution receipts and state notices. A per-call `maxOutputTokens` override changes
that budget; the 60 KiB reply byte backstop remains. There is no fixed combined
token cap for a model round. Context preparation and the final serialized-request
guard check the full request before dispatch, with the existing 16k compaction
reserve.

Preview truncation does not fail a successful cell or reset its heap. The result
identifies the preview and references the full generated output under the ignored
workspace `.run/js-output/` directory. Existing read/command tools can inspect that
file, and deliberately saved JS state remains usable. Files are written only for
oversized previews, with no automatic UI publication. Serialized JS returns have
a separate 1 MiB resource ceiling; genuine execution/resource errors still fail
explicitly. The baseline follows Codex and Pi's current 10k code-mode defaults,
while keeping our token estimate explicit rather than equating bytes with tokens.

## Bounded user inputs and workspace attachments

Prompt length uses a simple 32,000 Unicode character cap. The web composer turns
an oversized paste into a visible `paste.txt`; API, CLI and stdio callers get an
explicit error instead of silent truncation. Files default to 16 MiB individually
and 32 MiB combined, with a 16-file metadata guard.

The runner owns raw uploads and canonical message preparation. Immutable files
and integrity metadata live under the ignored workspace `.run/attachments/`;
the existing core persists the accepted message with references and bounded
text. A shared 10k-token envelope budget controls inlining, keeping large files
available to ordinary tools. This avoids binary storage in SQLite or a new
attachment subsystem in the harness. Native vision/document extraction and
retention cleanup remain outside this slice.

## Visible child controls and continuation (2026-10-13)

Child limits are visible to the agent (tool-call countdown) or opt-in (timeout);
no default caps; stale children trigger reminders instead of timeouts.

Every child profile accepts a caller-supplied output schema, one combined
per-turn tool-call budget, and an optional wall-clock timeout. Structured final
answers use the provider's native output format and are validated locally; one
correction with the validation error is allowed before a clear failure. A JS
wrapper and each tool invoked inside it count individually. The task states its
budget up front, every model-facing tool result reports the remainder, and
exhaustion forces a tools-disabled final answer rather than a surprise cutoff.
Calls exceeding the remainder receive explicit budget errors before any effect.

Timeout is measured in milliseconds and cancels only the current run through the
normal cleanup path. The child returns timed_out and any available partial answer.
All ended children remain continuable, including budget exhaustion, timeout,
manual cancellation and schema error. A follow-up resumes full history; it gets a
fresh budget or timeout only when explicitly supplied, otherwise unlimited calls
and no deadline. A new budget is stated in the follow-up. The output schema is
retained unless replaced or explicitly cleared.

The parent receives host notices for still-running children it has not interacted
with for agents.stale_reminder_secs (default two hours, zero disables), repeated
at most once per interval per child. Messages, status checks, waits and result
reads reset inactivity. The original no-auto-wake policy is superseded by the
2026-10-14 completion-wake decision above. Stale reminders themselves do not wake
idle parents. Child
orchestration and deadlines stay in the runner; core changes are small generic
run/dispatcher controls, with no store schema change.

## Reasoning of interrupted streams (2026-10-14)

Every reasoning artifact of a completed message is stored and never dropped,
including a thinking block cut unsigned at max_tokens (kept, marked
`unsigned_cut`, never sent to a provider). A stream that is cancelled or drops
before its response completes stores no assistant message, and its partial
reasoning, signed blocks included, is not kept. Reason: only complete messages
are persisted and stream fragments are transient; an interrupted turn cannot be
replayed, so its reasoning would be an audit record only, which would need a new
interrupted-message record. Supersedes the reading of the 2026-10-13 rule as
covering interrupted streams.

## Live provider apply (2026-10-14)

Provider setup must work in an already-running workbench. Saves, deletes and main
selection validate, prepare and apply immediately; external TOML edits use explicit
HTTP `POST /api/config/reload` or stdio `config.reload`. No watcher or general plugin
reload is added. Ordinary config layering and setup write targets remain unchanged.

Provider configuration is swapped at model-request boundaries. In-flight calls
finish with the old configuration; subsequent rounds, roles, children and workers
use the replacement. Explicit provider/model pins are preserved and fail clearly
when removed. History and reasoning artifacts are preserved. Failed preparation
keeps the active configuration. Replay origin guards include configured provider
route and endpoint as well as adapter/model identity. Missing route tags inherit
the current route when adapter/model match, following the store's pre-origin-tag
compatibility rule. URL/key-variable validation is shared by startup,
reload and setup; credentials resolve at apply time and never leave the host.

Named roles (including effort and approver threshold), web search/fetch backends,
web credential variable and cooldown also apply live. Other host configuration
still requires restart. The API separates disk
`main` from qualified `active_main` and startup `launched_main`; `reload_needed`
marks unapplied disk provider changes; `restart_needed` flags other disk settings
that require restart, remaining false for provider-only changes. The topbar
shows active truth; successful setup says "Saved and applied".

## Import copies spill and artifacts, 2026-10-21

The central store should be self-contained, so the one-time legacy import
copies each legacy store's `spill/` and `artifacts/` files into the central
store under the sessions' central ids and rewrites stored spill paths to the
copies. A legacy directory can then be deleted after its import, except for
child worktrees still in use, which stay where they are. The copy happens
before the source's receipt, so an interrupted import redoes it. A chat
imported under new ids drops the usage rows it shares with the original, so
reports count that history once. See [storage model](storage-model.md).
