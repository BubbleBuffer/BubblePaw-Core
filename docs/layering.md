# Layering

The split separates model access, in-memory runs and durable orchestration.
`bubblepaw-ai` and `bubblepaw-agent` exist; the durable shell stays in
`bubblepaw-core`. The layering guard keeps the boundaries measurable while the split
continues.

- `bubblepaw-ai` talks to models and knows no sessions or stores.
- `bubblepaw-agent` runs one run in memory and knows no database, HTTP or feature. Its API is unstable.
- `bubblepaw-core` makes runs durable and knows no model API, tool or feature word.

## The ai crate

`bubblepaw-ai` holds what passes between a harness and a model: the message,
content block, tool call and tool definition types, `ModelRequest`, `ModelResponse`,
`Usage`, the stream events, the cancellation token, the provider error and capability
types, the history cursor, wire shapes for cache accounting, and the `Provider`
trait. It depends on no workspace crate, and its dependency list is external crates
only. `bubblepaw-core` depends on it and re-exports every moved item at its old
path, so embedders import from `bubblepaw_core` as before.

The provider adapters (chat completions, Anthropic messages and responses), their
HTTP client, stream, retry and error handling, the request dump and the scripted
model live in `bubblepaw-ai/src/adapter`, behind the `http` feature. Only the
runner's `host` feature enables it, so a client-only build links no HTTP client. The
adapters reach the host through injected seams: a `ModelEgress` trait for proxied
egress, a `UsageSink` that the core capture scope installs, and a host-policy flag for
quota stops. `bubblepaw-runner` re-exports every moved name at its old path.

The extension provider, which speaks line-delimited JSON-RPC to a subprocess,
lives in `bubblepaw-ai/src/extension`, behind the `extension` feature; the runner's
`host` feature enables it. The provider builds the command and runs the protocol,
and a launcher the host passes to `ExtensionProvider::new` starts it. The launcher
returns the pipes and a `LaunchedProcess` that kills the whole process tree on
demand and on drop; the runner's launcher uses `bubblepaw-process`, so ai depends
on no workspace crate. The runner builds the provider with `extension_provider`,
reads the extensions file, and re-exports the moved names at their old paths. The
Claude Agent SDK rate-limit events the extension reports are parsed in
`adapter/quota_events.rs`.

The model catalog is `bubblepaw-ai/src/catalog.rs` with its data in
`catalog/models.json`, keyed by model name. It holds the dated list prices (the
`PriceTable`, `Rates` and `TokenClasses` types, re-exported by the runner's `pricing`
module), the largest synchronous output per model, the Anthropic cache lifetime, the
smallest cacheable prefix and the cache block size, and a context-window field that no
model fills yet. Each fact keeps its old matching rule. To update a price, edit
`models.json` and its `checked` date.

Still to move: the provider registry, which lives in `bubblepaw-runner` today. Some ai types carry a `session_id` that the ai layer
treats as an opaque key; the adapters add the `x-opencode-session` and Codex
cache-key plumbing, the replay-state and continuation code that handle that key,
and the extension provider keeps a cursor reset per `session_id` and sends
`session.close`; the guard records all of these occurrences as ceilings rather than
renaming them.

## The journal seam

The journal is two traits. `RunJournal` (`run_journal.rs`) is what the run loop and the dispatcher hold: replies, tool starts and finishes, outputs, consumed steering, the kept partial reply and the closing of unresolved effects are each one call, and the call commits its rows and durable events together. The loop reads pending input and duplicate call ids through the same trait. `SessionJournal` (`journal.rs`) is the shell's: admission, queued input, the end of a run, a continuation and a settled session, with `RunAdmission` and `RunFinish`. `run_journal.rs` names no store, run or client type and is held to the loop's vocabulary guard. The dispatcher writes through `RunJournal` too: `nested_dispatched`, `effect_unknown`, `effect_completed` and `effect_completed_for_session` are one transaction each, and the dispatcher holds no store. One implementation, `SqliteJournal`, implements both traits over the store and the event log; the shell holds it once for each. Writes that still ride inside a journal commit, such as the execution mode, tool surface and reminders bound at admission, stay in that implementation until they get their own seams.

The dispatcher in `bubblepaw-core` asks a `ToolGate` trait about each call instead of reaching into the approval code. `admits_without_write` says whether a call needs nothing written or asked, so the reply and dispatch still commit together; `before` returns allow, deny with the text the model reads, or cancelled, and may wait for a person; `after` shapes the finished result through the host's result hook. The call travels as plain data. One implementation, the stock gate in `approvals`, holds the policy, session rules, automatic review and workspace roots. The size-limit summary and the nested-call skip stay in the dispatcher. Tools and the result hook get no store from the dispatcher: the hook takes the call context, the result and the limit, and a tool that needs storage holds it from its registration.

The run loop asks a `RequestContext` trait what to send to the model: the history for the run, the draft messages with the steering batch and the preview of pending notes, the host preparation of that draft, the final request with its budget check, notes queued for the model, declared prefix changes, and the points where a request starts, is sent, finishes and is answered. It carries messages, requests, responses and the provider as plain data. One implementation holds the host preparation hook, the store and the context state. The overflow retry, the choice of when to remind and what to say, steering, and the tool surface stay in the loop.

The loop never holds per-request usage records. The request context hands it a provider that captures them around `generate`, settles them when the request ends, and keeps them per run; the journal writes whatever a run has waiting in the commit of its reply or of its end, and the records are let go with the run when it ends unwritten.

The run loop and the dispatcher show live events through the `LiveEvents` trait, passing a `LiveEvent` enum (tool starts and finishes, paragraphs, request starts, text and thinking deltas): `transient`, `progress` and `paragraph`. `live_log.rs` maps `LiveEvent` to the event log's kinds. Live events reach subscribers only and are never stored; durable events stay with the journal. The stock implementation is the event log. A `ToolDispatcher` is built from a journal, a gate and a live sink (`from_seams`); `new` builds the stock three from a store.

## The run loop and its shell

`Harness::drive` is the shell around a run: the active map, cancellation attribution, the stopping flag, the pending-input check, the run checkpoint, continuation into a new run, finalizing, `runtime_idle` and the single-tool completion all live there and take the runtime lock. `run_loop.rs` holds the loop for exactly one run. `run_once` takes a `RunInputs` (the run, its options, the cancellation token and the single-tool record), runs the rounds of `turn`, and returns a `RunOutcome`: status, text, usage, error, result message id and final message. On failure it first keeps the partial reply and closes the run's open records through the journal. `run_settle.rs` holds the second half of a round: the checks on the model reply, its recording, tool dispatch and steering. The loop reaches the tools of a run through a `ToolSource` trait and holds no store. `open_run` returns the run's tools, its saved prompt and declarations, and whether every round asks again; `round_tools`, `surface_changed`, `queue_round_reminders`, `bound_result` and `result_required` are the other questions and reports, and the implementation does the store reads, the catalog and result notices and the execution mode behind them, when it did before. The shell builds the tool gate once per run and passes it in `RunInputs` with a flag for whether the gate is told the user's latest request; the history fallback for that request sits behind `RequestContext`.

The shell builds everything a run needs. `RunLoop` (in `run_inputs.rs`) holds the journal, the live sink, the request context, the provider binding, the observation value and a small `LoopLimits` (turn start hook, default round limit, request and paragraph byte limits); it is not a `Harness` and reads no config, runtime state or store. `RunInputs` carries the per-run things: the run, its options, the cancellation token, the single-tool record, the `ToolSource`, a builder for the run's dispatcher and whether the gate is told the user's latest request. The shell builds the gate and the dispatcher (approval setup, tool budget and interactivity included), so the loop files do not mention approval; they ask the dispatcher for the contexts they need. The dispatcher knows no approval type: the shell hands it one opaque hook that prepares each call context for its session, and the hook writes the run's policy and its resolver into the context's `Extensions` (a type-keyed map on `ToolContext`). `ApprovalPolicy::of` and `ApprovalPolicy::resolver_of` read them back; an unset slot reads as the default policy. The provider binding is a resolver the loop calls each round. The history cursor sits behind `RequestContext`, which forgets it on release. The partial reply is kept by the loop through the journal. A guard in the layering test keeps `run_loop.rs` and `run_settle.rs` free of the harness, approval, sqlite, store, quota and child vocabulary, and `dispatch.rs` and `dispatch/calls.rs` free of approval. `Message::new` in the ai crate builds the messages the loop writes.

## Rules

Normal and build dependencies obey these rules; dev-dependencies are exempt:

- AI and process depend on no workspace crate.
- Agent depends only on AI among workspace crates, and not on `rusqlite` or `reqwest`.
- Core depends only on AI and agent among workspace crates, and not on `reqwest`.
- No workspace crate depends on SDK; only SDK depends on runner.

The guard skips rules for crates whose folders do not exist yet. Existing forbidden
dependencies are listed in `layering-allowlist.txt` and must be removed from the
list when the dependency disappears. New violations fail.

The guard counts case-insensitive substrings in every Rust source file, including
comments. Core and agent track `episode`, `chapter`, `child`, `opencode`, `quota`
and `"js"` (including the quotes). AI tracks `session`, `rusqlite`, `approval`,
`episode`, `chapter` and `child`. Missing entries have a ceiling of zero.
An increase fails and reports the ten files with the most hits. When a count falls,
lower its ceiling in `layering-allowlist.txt` to the new total; never raise it.

## The agent crate

`bubblepaw-agent` holds the files that make up one run in memory: the run loop
(`run_loop.rs`, `run_settle.rs`, `run_inputs.rs`, `overflow.rs`, `model_stream.rs`,
`stream.rs`), the tool dispatcher (`dispatch.rs`, `dispatch/calls.rs`), the tools and
their registry (`tool.rs`, `tool_registry.rs`, `tool_result.rs`, `tool_views.rs`,
`tool_notices.rs`, `discovery.rs`, `submit_result.rs`, `script.rs`), and the plain data
and traits between them (`run_journal.rs`, `request_seam.rs`, `tool_source.rs`,
`tool_gate.rs`, `live_events.rs`, `observer.rs`, `loop_hooks.rs`, `loop_options.rs`,
`loop_records.rs`, `run_data.rs`, `prepare_types.rs`, `reminder_types.rs`,
`round_reminders.rs`, `execution_mode.rs`, `extensions.rs`, `ids.rs`). Its modules are
flat and public so that core can reach them; the crate root re-exports the common
names. It depends on `bubblepaw-ai` and on ordinary libraries (anyhow, async-trait,
chrono, jsonschema, serde, serde_json, tokio, uuid) only. `bubblepaw-core` depends on
it and re-exports every name it exported before, at the same path (`runtime::PrepareMode`
included), so the runner, MCP, SDK, remote, process and client-check crates and
external embedders compile unchanged. Embedders should keep importing from
`bubblepaw_core`.

What the loop needs from the shell comes through traits and plain types: `RunJournal`,
`RequestContext`, `ToolSource`, `ToolGate`, `LiveEvents`, `Observer` and `ToolWatch`,
`LoopOptions` (model, round and token limits, output schema, script) and
`SingleToolRun` (call and result only; the shell keeps the completion channel in its
own wrapper). The plain-data structs the shell builds or reads (`RunLoop`,
`RunInputs`, `RunOutcome`, `LoopLimits`, `RunTools`, `ToolSurface`, `SingleToolRun`)
have public fields; the registry's fields and the moving set's private state stay
`pub(crate)`. `ToolDefinition::new` and `Message::new` in the ai crate build the
definitions and messages the loop writes.

`bubblepaw-core` keeps the shell halves beside `runtime.rs`: `stock_tool_source.rs`,
`run_loop_build.rs` and `live_log.rs`, the journal implementation, the approvals, the
store and everything durable. Its `lib.rs` aliases the moved modules (`crate::tool`,
`crate::dispatch` and so on) so the shell's code keeps its paths.

Two guards in `tests/runtime/layering.rs` and `layering_agent.rs` protect the crate. The dependency rule
above keeps `bubblepaw-agent` to `bubblepaw-ai` among workspace crates and forbids
`rusqlite` and `reqwest`. `agent_crate_holds_no_shell_vocabulary` keeps every file
under `crates/bubblepaw-agent/src` free of the shell's vocabulary (harness, approval,
sqlite, the store, the event log), except lines the model reads, which are listed in
the test with the reason; `child` and `quota` are held to zero by the word ceilings.
The Rust module graph already stops the crate from importing core.
