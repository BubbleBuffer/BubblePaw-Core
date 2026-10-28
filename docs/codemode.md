# Tool execution modes

**Status:** native default and opt-in JavaScript wrapper, 2026-10-22.

The Rust harness owns sessions, cancellation, tool dispatch and explicit durable
outputs. The runner supplies an embedded QuickJS adapter. It is a language for
composing Rust capabilities, not a Node/npm environment. Rust tools and frontend
renderers can extend the boundaries independently; no package/plugin loader is
included in this version.

## Execution modes

Native mode is the default for new chats. It advertises
the registered native capabilities (`read`, `list`, `search`, `edit`, `write`,
`exec`, process waits and host additions) as top-level model tools. It reuses
their existing Rust implementations, schemas and dispatch boundaries.

```toml
[agent]
execution_mode = "native"
```

Set `execution_mode = "js"` to advertise only `js` and call the same native
capabilities through the JavaScript wrapper. Native mode never declares `js`.
`enable_js` has been removed; configurations containing it fail with guidance
to use `execution_mode = "js"`. Ordinary config layers work, including
`--set agent.execution_mode=js` and `BUBBLEPAW_AGENT__EXECUTION_MODE=js`.

Call independent tools together in one message.

Each chat records its initial execution mode. Resume uses that stored mode,
even after the config default changes. Legacy chats without a stored mode use
JavaScript. New root chats bind to the configured default on first admission or
an explicit execution-mode lookup. Child chats and history forks inherit their
parent's stored mode.

Embedders configure `HarnessConfig.execution_mode` with
`bubblepaw_core::ExecutionMode::{Js, Native}`.
Register native capabilities and `CodeMode` as before; the harness selects the
model-facing surface from those registrations. Mode selection does not grant a
capability the host did not register. Explicit native mode also takes precedence
over a provider's legacy preference for the JS-only tool surface.

## Structured results

`RunOptions.output_schema` enables a run-local core `submit_result` tool with
that exact input schema. The chat retains its initial declaration, or none if
its first run had no schema. A later run with a new or different schema gets a
run-start host notice containing the full current definition. Call it directly
or through `call_tool`; both validate against the current run's schema. A run
without an output schema refuses submission with a normal tool error.
After the contract first changes, every later run announces its current contract,
including a return to the original schema or to ordinary text output.
If apply covers that notice during a structured run, the host repeats the current
definition after apply.
Providers receive no response schema. Call `submit_result` to finish;
in opt-in JS mode use `await tools.submit_result(value)`. The accepted arguments
are serialized as the run's final JSON text for existing report consumers.

Invalid arguments produce a normal tool error and can be retried. A text-only
turn gets one host reminder; a second text-only turn fails the run. Other calls
in a native message run before submission is accepted. A JS cell finishes before
its accepted result completes the run. The first valid submission wins; further
submissions return a tool error. Work-tool budget exhaustion still permits the
completion tool and its access wrappers, while ordinary targets remain blocked.
Submission ends the current run; queued input is handled by the next run.
Summary and chapter forks refuse `submit_result` and use their own fork-local
return instructions.

## Shared calls and native discovery

Native calls, JavaScript `tools.<name>()`, the model's `call_tool` and the
embedder API use one dispatcher. It validates the effective run allowlist,
availability and arguments before approval and the effect boundary. Calls share
the run's tool budget. Wrappers and their targets each consume a call; wrapper
access does not grant access to a denied target. Origin labels affect tracing
only. Unknown tools return `Unknown tool: <name>`; interrupted effects are never
replayed automatically.

Native mode always declares `tool_search({query, limit?})` and
`call_tool({name, arguments})`, including in restricted runs. Discovery matches
names, descriptions and original MCP server names, and returns descriptions and
input schemas from the allowlisted, read-only view. `select:name,other` loads
exact schemas. Call declared tools directly; use `call_tool` for tools found
with `tool_search`.

Built-in host tools are core and declared in native mode. MCP tools are deferred
by default: they remain callable but their schemas are discovered on demand.
Set `defer = false` under an MCP server to declare its tools as core. A short
system-prompt anchor names each effective deferred server, its tool count and a
few tool names, with instructions to search and call it. Anchors contain no
schemas and are absent when nothing is deferred.

Declarations and the frozen system prompt are persisted for the whole chat,
including later runs, process restarts and summary applies. The initial prompt
includes the initial deferred-server anchors. Reloads and published MCP
connections/disconnections update the callable inventory for the next request
and append a generated harness reminder. Added or updated core tools include
their full name, description and complete input schema as compact JSON. Deferred
MCP tools include only their names, original server names and a `tool_search`
hint; their descriptions and schemas stay out of the notice. Removals list names
only, and each change group is sorted by tool name. Newly available tools are
reached through `call_tool`, with discovery first when deferred; the next request
retains the same tools array and system prompt. Generation changes retain an
informational `context_change` record with reason `tools_reloaded` and
`prefix_changed: false`; they do not declare a cache bust. Newly added tools stay
behind `call_tool` for this chat, even when they are core. A new chat declares the
current core tools. The model's
`reload` tool and `/reload` refresh the inventory, not the prefix; this checkout
has no separate full-prefix reset path. JS declarations stay frozen too, while
its calls and discovery use the current allowed inventory.

When a summary applies, the cold prompt receives current deferred-server anchors
and changes since the chat's initial inventory. Deferred additions carry names
and server names; undeclared core additions carry full definitions and compact
JSON schemas. Removals carry names only. This catalog is sorted and persists
until the next apply. Updated definitions use the same core/deferred forms, so
their current schemas survive after the earlier notices fold away with history;
the declarations and frozen system prompt do not change.

## Public embedder APIs

`Harness::effective_tool_definitions(session_id, &RunOptions)` returns
`Result<Vec<ToolDefinition>>`. Definitions contain each tool's name,
`input_schema` and description, including registry-derived JS help only when
the chat uses JS mode. Use this instead of rebuilding declarations from
`ToolRegistry::capabilities()`. The session's read-only policy selects the
initial declarations. Later calls reuse that chat snapshot; `RunOptions.tool_names`
still filters live discovery and dispatch. Before the first admission this API
previews the initial declarations without persisting them. A run with `tool_names`
refuses any other target at call time: the error names the allowed tools, and the run
continues. The run states its allowed tools once, in a `tool_restriction:<run>`
reminder queued at its first model request. Access wrappers remain available;
every wrapped target still passes the same gate. As in
model turns, approval-denied tools remain declared and are refused at the call
gate. The API describes the initial request; later budget enforcement can disable
work tools while retaining their wire declarations. Host context adapters cannot
replace the chat's frozen declarations or system prompt.

`Harness::call_tool(session_id, name, arguments, RunOptions, CancellationToken)`
is async and returns `Result<ToolResult>`. It invokes one registered tool
without a provider turn. Session admission, argument validation, cancellation,
tool budgets, approval policy, result limits, journal messages and effect
boundaries use the normal execution path, including deferred host capabilities.
It cannot bypass `tool_names`. The host still owns authentication and the
mapping from its caller to the session ID.
Completion belongs to this invocation's run: a later model run on the same
session cannot delay its result or replace its final status.

An interactive approval returns a clear error instead of waiting for a person;
this also applies to native calls nested inside JS. Existing explicit policy
allowances apply, while configured denies and the hardline floor still win.
An `auto` decision that would need an approval model is also refused so the call
stays model-free; an existing session allowance still applies.
Tool failures retain `ToolResult.is_error`; `pending` retains its normal meaning
for host-owned asynchronous effects. An unsuccessful admission or cancelled run
can return an outer error. Durable history and effect records remain available
through the store, and unknown outcomes are never automatically retried.

Calling `js` with `{"code":"return 1 + 1"}` uses the same cell execution and
result shape as `RunOptions.script = Some("return 1 + 1".into())`. The chat must
use JS mode and the adapter must be registered. New CLI `script` chats likewise
need `--set agent.execution_mode=js`. A single-tool call never turns queued
input into a later model request.

## JavaScript API

`js({environment?: "main", code: string, maxOutputTokens?: number})` executes a fresh async cell. Only the
explicit `state` object survives between cells. Tool names come from the Rust
host capability registry: `tools.read`, `tools.exec`, etc. In opt-in JS
mode, `js` is the runner's only model tool; native capabilities are reached
through it. Its description, `tools.$list()` and `tools.$search()` use the same
effective run snapshot after allowlist and read-only filtering. `$search` shares
the native discovery matcher; deferred tools are callable as `tools.<name>()`.
Their argument schemas still validate calls composed in JS. Native
failures reject their promise. Old `workspace_*` and `process_*` aliases are
removed; stored history remains readable and is never re-executed on load.

`state`, `tools`, `text`, `file`, `image`, `wait` and `cmd` are available from the
cell's enclosing scope. Ordinary local declarations may shadow those names;
helpers captured in saved callbacks remain tied to their original cell.

```js
const read = await tools.read({path: "README.md", startLine: 1, lineCount: 40});
state.lastRead = read;
return read.content;
```

An expression cell, such as `await tools.exec({command: "Get-ChildItem -Force -Name"})`,
returns its value as an ordinary model tool result. Multi-statement cells use
explicit `return` to select the result. Returning data does not publish a user
message. Use `text(markdown)` deliberately when presenting a result, including
Markdown assembled from variables. A cell without a return still receives an
execution/publication summary. Intermediate native results remain inside JS.
Image reads are the exception: `await tools.read({path:"diagram.png"})` attaches
supported image input to the `js` result automatically, even when the cell returns
another value. Images are deduplicated, limited to four per call and 5,000,000
bytes each, and never resized. Omission notes identify the limits. The bounded
image-reference metadata is appended outside the text-preview token budget;
original bytes stay in SQLite and are encoded only for the provider request.
See [provider image input](providers.md#image-input-2026-10-15).
Defined returns are JSON values. The entire model-facing reply defaults to
10,000 estimated tokens using O200kBase, including its result, execution receipt,
preview references, error text and state notices. `maxOutputTokens` overrides the
per-call budget with an integer of at least 256; invalid values fail before the
cell runs. A 60 KiB serialized UTF-8 backstop still applies, under the dispatcher's
64 KiB limit. There is no aggregate model-round output-token cap.

As of 2026-10-20, this reply budget applies only at the model boundary. A host
call inside a cell returns its complete typed result to JS without dispatcher
previewing or spilling, so fields such as `stdout`, `capture`, `cursor` and
`exitCode` remain available. The tools retain their own output limits, including
16 KiB per-stream exec previews and complete exec-output spills above 64 KiB.
The existing 2 MiB serialized host-return ceiling, 8 MiB serialized cell-return
ceiling and 32 MiB heap limit still apply. Return only the fields the model needs
to keep the final reply small.

An absent optional field is `undefined` in JS. Returned objects omit own enumerable
data properties whose value is `undefined`, including nested optional fields such
as `diagnostic: result.diagnostic`. Explicit `null` is preserved. Array elements
must still be defined; sparse arrays, accessors, non-enumerable properties,
unsupported prototypes, nonfinite numbers and cycles fail explicitly. Encoding
does not invoke getters or `toJSON`, and omitted fields still count toward the
structure limit. A top-level `undefined` or no return gives an execution receipt.

A large successful return yields an explicit JSON preview envelope rather than
failing or resetting the heap. Small replies retain their existing API shape.
The envelope has `truncated:true`, `previewFormat:"text"`, a marked head/tail
`preview` string, `stateChanges`, `metadataTruncated`, and
`fullOutput:{path,bytes,format:"text"}`. The preview string is an excerpt, not
parseable complete JSON. Notice metadata also shares the budget; omitted notice
metadata sets `metadataTruncated:true`. Full replies are saved to generated
files under `<data_dir>/spill/<chat-id>/<id>.txt`. References contain the absolute
path, total UTF-8 bytes and `format:"text"`. Here `chat-id` is the owning
session ID; each child session owns a separate folder and retention allowance.
Tool replies dispatched directly to the model over the dispatcher 64 KiB cap
use the same store and a marked head/tail preview, preserving pending and error
flags. Nested host results are not subject to that model reply cap. These are host output files, not user publications or immutable
UI artifacts.
Their text is the complete serialized result followed by any `[---]` notice
suffix. The reference remains usable after the next cell or an environment reset.

Use existing `tools.read` with `startLine` and `lineCount` to inspect the saved
text. A chat may read only its own spill folder through this exception outside
the workspace; `list`, `search` and patch permissions stay workspace-only. For a
single long serialized JSON line, use `tools.exec` to process the saved file
locally and return a selected result. Native read/process previews retain their
own bounds. For example, when a saved reply fits `tools.read`:

```js
const saved = await tools.read({path: "/absolute/data/spill/<chat-id>/<id>.txt"});
if (saved.truncated) throw Error("Use tools.exec to process this file locally");
const full = JSON.parse(saved.content.split("\n[---]\n")[0]);
return {count: full.items.length};
```

Save a value in `state` before returning it to keep the full original object
available for later JS processing. Spill failures report `fullOutputError` without
claiming full access; successful cells still keep their state. Files survive host
restart, while JS state remains volatile. Generated files have a 256 MiB
per-chat retention cap, evicting the oldest files first. A single output exceeding
that cap is not saved and reports `fullOutputError`; its receipt never claims
complete access. Chat deletion removes its spill folder, including deleted
descendants; archiving keeps it. Filesystem retention is separate from SQLite
transactions. Underlying serialized JS returns have a separate 8 MiB resource
ceiling and the existing 32 MiB heap bound.
Cyclic, unsupported or resource-limit returns still produce correctable errors
and the normal failed-cell heap reset. Runtime errors never retry the cell.

Prefer optimistic batching of independent, likely-needed reads and checks within
one cell. A few extra useful reads are preferable to additional model round trips.
Use `Promise.all` and return compact labeled results; dependent steps and mutations
stay ordered. Start another model turn when the observations determine what to do
next, rather than wrapping every native call in its own JS call.
For speculative probes that may fail individually, use `Promise.allSettled` and
return labeled successes/errors so one missing file does not discard the batch.

```js
const paths = ["README.md", "AGENTS.md", "Cargo.toml"];
const docs = await Promise.all(paths.map(async path => {
  const read = await tools.read({path});
  return {path, content: read.content};
}));
state.docs = docs;
return docs;
```

`state.reset()` clears saved state data inside a cell. It does not reset the JS
heap or terminate processes. Results carry a compact notice after `[---]` when
state changes, separately from the returned data. Notices name keys without
copying their values; a reset is explicit even when state was already empty.
The notices are model context, not separate user-facing publications.
They summarize net changes over the cell. Nested edits to plain objects/arrays
mark the owning top-level key changed; observation uses property descriptors and
does not call getters or `toJSON`. Traversal and notice size are bounded, and an
explicit observation-limit notice covers state that cannot be fully inspected.
The reserved `reset` method is non-enumerable and excluded from returned state
data. If user-defined non-configurable properties prevent clearing, reset fails
before deleting any keys; the normal failed-cell heap reset is then reported.

The native JS methods are `tools.agent`, `tools.exec`, `tools.poll`, `tools.cancel`, `tools.read`,
`tools.list`, `tools.search`, `tools.edit`, `tools.write`, `tools.environments`,
`tools.history_chapters`, `tools.history_search` and `tools.history_read`.

The five workspace file tools are `read`, `list`, `search`, `edit` and `write`.
`edit({path, old_string, new_string, replace_all?})` performs exact literal
replacement, unique unless `replace_all` is selected; it returns
`{path, replacements}`. `write({path, content})` creates or overwrites a UTF-8
file, creates parents and returns `{path, bytes, created}`. Neither requires a
prior read or hash. Both use staged writes, compare-and-swap, checkpoints and the
ordinary approval gate. Their signatures are always shown when available, and
child workspaces rebind both. These are host tools within `js`; top-level
provider file tools are outside this scope. See [file tools](file-tools.md).

`read` recognizes PNG, JPEG, GIF and WebP by their bytes. Providers without image
input receive a short text note. The `image(...)` helper below remains an explicit
presentation output; see [providers](providers.md).
With web enabled, `tools.web_search` and `tools.web_fetch` are also available.
Start visible research children with `tools.agent({profile:"research", task})`
and follow up through `tools.agent_send`. See [web tools](web.md).
Search/fetch calls suspend the non-wait idle deadline as operation waits; their
network deadlines and cancellation still apply.
Positive `tools.poll({handle,yieldMs})` calls are also operation waits. Their
requested window is capped at 30 seconds; omitted or zero `yieldMs` inspects
immediately. For completion without repeated polls, use `wait({handle})` and
then inspect output. Cancellation remains active throughout either wait.
The history methods are read-only, scoped to the calling session, and address
canonical messages by 0-based index. For their arguments and result limits, see the earlier chapter design
kept in the project notes.
`Object.keys(tools)` shows the methods actually available under host policy.
`wait` is a cell helper backed by a Rust capability. Each nested call uses the
shared dispatcher, validates its schema,
inherits the allowed capabilities and receives a distinct invocation identity.
Nested read results are not independently stored in model history. Necessary
write/process effects retain durable boundaries. Process output progress stays
in bounded RAM.

The core library stays independent of JavaScript. `ToolRegistry::register`
registers a model entry point; `register_host` registers a native capability.
Model definitions and host definitions are separate, without hidden-name flags
or alias selection. `RunOptions.tool_names` restricts which model entry points a
run may call, enforced when a call is made; declarations stay those of the
profile. Host capability policy is set by the host and read-only tool filtering. Calling
`js` does not grant capabilities that the host has withheld. Provider attempts to
call a native capability receive a correctable error before any effect begins.

## Child agents

The runner provides long-lived child sessions through ordinary JS host calls:

```js
const job = await tools.agent({
  task: "Explain how the app works from these files.",
  context: state.docs
});
state.explanationJob = job;
const result = await wait({handle: job.handle});
if (result.status !== "completed") throw Error(result.error ?? result.status);
text(result.text);
return {sessionId: result.sessionId, usage: result.usage};
```

A finished child is idle and accepts another turn:

```js
const next = await tools.agent_send({sessionId: state.explanationJob.sessionId,
  text: "Now explain the error handling."});
const result = await wait({sessionId: next.sessionId});
text(result.text);
```

`tools.agent({task, context?, profile?, access?})` returns
`{handle:"agent:<id>", sessionId, status:"running"}` after durable admission.
Context is any JSON value; task and serialized context together fit within 64 KiB.
The child uses a registered agent profile, a fresh conversation and separate
JS state. It receives only the explicit task/context and its tool reference.
`profile` defaults to `"read-only"`, which excludes commands and writes.
Request `profile:"workspace"` for those capabilities; a read-only host omits
that profile. The deprecated `access:"read-only"|"workspace"` alias still selects
the corresponding profile. With web enabled, `profile:"research"` provides only
search, page reading and JS. Profiles can override the inherited provider/model
and round limit. Children receive agent tools at every depth by default. An
explicit `agents.max_depth` cap restricts them to depths below that limit.

`wait({handle|sessionId})` returns turn `status:"completed"`, `"failed"` or `"cancelled"`,
with `text`, `sessionId`, and available `usage`/`error`. A `timeoutMs` returns
`status:"timeout"` without stopping the child. `tools.cancel({handle})` cancels
and awaits its subtree. Every child-addressed tool also accepts `sessionId`,
but only for a catalog child of the calling session. Agent waits use the same
explicit-wait/backgrounding mechanism
as commands: steering releases the parent foreground while the child continues;
stopping the parent also stops its subtree deepest first and awaits cleanup,
including after the parent run ends.

`tools.agent_send({handle|sessionId, text})` steers a running child immediately
or starts a new turn when idle. Cancelled and failed children reject input.
`tools.agent_status({handle?|sessionId?})` returns one child's state and available
result. With no address it lists all children of the calling parent, with state,
handle, sessionId and `unread`. A result stays unread until `wait` or a status
call for that specific child returns it; listing children does not consume it.
An unretained child's listing has `handle:null`; address it by `sessionId`.
Read receipts and cancellation intent survive handle eviction and restart.
Children continue concurrently and results never automatically wake the parent.

Child publications and final answers are saved in the child's ordinary session.
They do not publish into the parent; the parent chooses what to present with
`text()`. A finished turn leaves the child `idle` and reusable. Its remaining
commands are cancelled and JS environments closed before the turn result;
resources are recreated for the next turn. Cleanup failure
returns a failed reply with `runStatus` and an explicit error, leaving the spawn
effect unresolved; it does not claim orphan cancellation succeeded. Each spawn
and idle send returns `pending` and settles its parent effect only after the turn
ends and cleanup succeeds. A running steer settles immediately. Admission errors
return ordinary error results rather than pending work.

The layered `[agents]` caps `max_depth`, `max_active_per_parent`, `max_active_total`
and `max_retained` are unset by default, meaning no limit. Optional caps are
available against runaway recursion. Active caps count running turns, including
sends, across all depths. A retention cap expires older inactive handles first;
those sessions remain available by `sessionId` and in status listings. Replies fit within
60 KiB of serialized JSON; truncation is explicit and the full answer remains
in child history. Handles are process-local; after restart, catalog discovery
reattaches a saved child by `sessionId` and gives it a fresh handle. Interrupted
effects become unknown through core recovery, and the next send resumes marked
history without replaying them.

Child runs receive an explicit approval policy clamped against the calling run's
effective policy, including its override. The stricter parent/profile mode wins
for confirm and per-tool modes (`Deny < Ask < Auto < Allow`); the parent timeout
is inherited. Child requests remain visible on the shared host event log.
Child orchestration lives in the Rust runner, above the core harness, with no
subprocess, scheduler, store schema change or new frontend manager.

## Commands and waits

```js
state.job = await tools.exec({
  command: {parts:["python ", ""], values:["generate_report.py"]},
  cwd: ".",       // workspace or an absolute scratchpad path; default .
  background: true // return at once; omit to block until exit or exec.sync_timeout_secs
  // timeoutMs is an optional positive command deadline that kills; absent means none
});
return state.job;
```

Commands are sync by default and background on sync timeout, user action or steer
(`status:"backgrounded"`); see [exec](exec.md) for jobs, notices and limits.
Use argv for single programs; it spawns directly without a shell. Use strings
for pipes, redirects and `&&` chains: `cmd.exe /d /s /c` on Windows and `sh -c`
on Unix. There is no per-call shell option, PTY or interactive stdin. A result includes `status`, `handle`, `stdout`, `stderr`,
`cursor:{stdout,stderr}`, and per-stream capture/gap metadata. Terminal results
include `exitCode`. Status is `running`, `backgrounded`, `exited`, `cancelled` or `timed_out`.
Handles are session-owned and process-local; an environment is not filesystem
or process isolation.

### Argv command templates

```js
const glob = "**/*";
const query = "long schlong";
const result = await tools.exec({command: cmd`rg --glob ${glob} -e ${query}`});
return result;
```

`cmd` is a JavaScript tagged template. QuickJS supplies the literal parts and
values separately. The helper preserves raw literal parts and stringifies only
strings, finite numbers and booleans. Rust validates that structure and launches
one native executable with separate arguments, rather than executing a rendered
shell command. The quoted rendering is display-only. The native form is
`{parts:["python ",""],values:["script.py"]}`; the tag builds the same object.

This v0 tag accepts a literal executable, plain literal words, and complete
unquoted substitutions. Static quotes, embedded-word substitutions, newlines,
shell expansion, pipes, redirects and comments are rejected. Use ordinary strings
for pipes, redirects and `&&` chains. Put script source in a scratchpad file,
then run it with argv in the same cell or native batch; no `python -c`, `node -e`
or inline PowerShell. PowerShell runs only as `powershell -NoProfile -File <file>`. Put a spaced literal argument in a substitution.
Arrays, objects, null, nonfinite numbers and NUL are rejected. Windows shell
builtins/aliases and batch scripts require ordinary shell strings. The guarantee
is argument boundaries, not the semantics of a program's flags or regex syntax.

Template descriptors remain internal values and are observable through optional
[behavior tracing](tracing.md). This is experimental and not a cross-shell parser.

```js
const next = await tools.poll({handle: state.job.handle, cursor: state.job.cursor});
state.job = next;
const outcome = await wait({handle: next.handle, timeoutMs: 60000});
return outcome;
```

Each reader supplies its own cursor. Polling does not consume another reader's
output. Each stream reply keeps a bounded prefix/tail of at most 16 KiB of captured
bytes, reserving room for visible `omitted bytes` markers when ranges are missing.
Capture metadata and cursors refer to original stream byte offsets; markers are
display text, not original process output. Check `capture.gaps` and `status` before
parsing structured output. Lossy UTF-8 decoding is explicit. A wait timeout
returns `status:"timeout"` and leaves the command running. With no explicit timeout, a
process handle wait uses 600 seconds; `timeoutMs` may raise or lower it. `await wait({ms:120000})` is a deliberate
delay. Wake resolves time waits early with an explicit `woken` result; it never
force-completes an operation wait. Cancellation remains active throughout.

Exceptional process replies include a bounded `diagnostic`: nonzero exit and
stderr, command timeout/cancellation, omitted bytes, capture errors or decoding
loss. Nonzero exit remains a process result so callers can handle expected cases
such as `rg` returning no matches. A complete successful result adds no diagnostic.
If a JS cell throws after a problematic process outcome, its error includes that
last outcome without copying raw stdout. Invalid tool arguments identify the tool
and JSON field; launch/host failures retain their exact cause.

For a complete large tree/report, redirect the command's output to a workspace
file and publish `await file({path})`. The process reply is an output preview,
not an unbounded data transfer channel. File captures remain limited to 16 MiB.

Ordinary host calls have a 30-second deadline in CodeMode, independently of
explicit waits in the same cell. Each cell has a 3-second cumulative active-JS
budget; awaiting does not spend it. Waiting environments block cheaply on their
completion channel with a short cancellation heartbeat.

## Named environments and user interruption

One `main` environment is created lazily per session. Each environment has one
active cell, its own heap/generation and bounded state-key/type/primitive previews.
There are at most four environments per session and sixteen total. Each heap is
limited to 32 MiB with a bounded stack; up to 64 host calls can be pending.

`await tools.environments({action:"create",environment:"diagnostic"})` creates an isolated
JS heap that shares session-owned native handles and the workspace. `inspect`,
`reset`, `close`, `cancel`, `background`, `wake` are explicit controls. Reset/close
of a busy environment requires cancellation. These are backend/API controls;
the basic web frontend does not expose environment management. Inspection does
not invoke user getters or `toJSON` functions. A cell cannot reset, close or cancel
its own active environment; use another environment or host control for that.

Ordinary steering is accepted durably and joins the next main model request
without releasing foreground waits. Explicit environment or host background
controls release waiting cells; sync-exec-only waits return a `backgrounded`
command result while their cell finishes normally. A detached cell's
initial model call returns honest background metadata, allowing the model to
investigate in another environment. The original continuation retains its own
execution/output identity and can finish later. Cancel is separate and stops owned
active work, including commands launched by an earlier completed cell. A completion
record remains attached to the original run even if the conversation has moved on.
When a background cell completes, named `tools.environments({action:"inspect",
environment:"main"})` exposes its volatile `result` and `result_execution_id`.
The volatile result lasts until the next cell or reset; large results retain their
marked preview and spill file reference. Environment listings and actions show each
environment's status and its outputs whole, up to 24 KiB of outputs per environment;
later outputs are dropped and counted in `outputs_omitted`. Background receipts, named inspection and combined
environment inspection all obey the reply budget; inspection may itself return
a marked preview envelope. No inspection path re-exposes an unbounded return.

## Output and durability

The experimental table helper has been removed. Existing archived tables remain readable in saved history.

### Existing outputs

`text(markdown)` emits Markdown. `await file({path,name?})` and
`await image({path,alt?,name?})` explicitly capture a workspace file and return
`{id,name,mime,size}`. Commands do not discover artifacts automatically. Captures
are immutable and session-owned, limited to 16 MiB each. Supported image outputs
are PNG, JPEG, GIF and WebP. Custom executable HTML/SVG/JS outputs are not enabled.

Explicit outputs are bounded JSON records in SQLite. Captured bytes and bounded
metadata sidecars live in `artifacts/` beside the archive. Each cell can emit up to
64 outputs and 192 KiB of output data; archive bounds also limit record size and
total bytes. Published outputs return bounded previews/references; ordinary JS
return values are separate model tool results, not publication previews. The
frontend uses Markdown and registered basic
renderers, with original run ownership. Presentation output does not automatically
add image bytes to model context.

The schema v1 -> v2 migration is transactional. Pending asynchronous effects remain
open until terminal observation, block forks while active, and become unknown after
host restart. Unknown outcomes are never automatically replayed. JS heaps and
native handles are volatile; restart/fork starts with fresh heaps. Failed/cancelled
cells also reset their environment and increment its generation. No heap replay,
automatic file rollback, completed-JS state cloning or frontend plugin loader.

## Console diagnostics (2026-10-20)

`console.log`, `console.info`, `console.warn` and `console.error` collect one
string per call, joining arguments with spaces and JSON-encoding non-strings.
They do not publish output to the user. A cell that logs returns
`{result, logs, logs_omitted}`; its `result` is the ordinary returned value or
execution receipt. A failed cell carries `{error}` in that result. Cells without
logs retain their existing reply shape.

Collection is bounded to 256 entries and 16 KiB of text. Complete lines that do
not fit are omitted; `logs_omitted` counts them, including lines dropped to fit
the cell's existing reply token/byte budget. A truncated result receipt retains
the omission count. Logs reset for each cell and saved console callbacks retain
the original cell's authority. Syntax and runtime errors include the original
cell line and column when QuickJS supplies a location.

Host argument validation reports valid argument names when a call contains an
unknown name. Missing workspace paths identify the requested path.
Native descriptions stand alone and avoid JavaScript invocation syntax.

## Tool description and default instructions (2026-10-22)

The `js` description is short how-to text, not a reference spec. Its fixed part
covers the cell model, a few working-well rules and three worked examples
(parallel reads, edit then exec, background exec with `wait`). Always-shown
tools get signatures and first-sentence summaries from their native descriptions.
Examples only name effective capabilities. Other tools are named and their
schemas come from `tools.$search`. The description stays under 3.5 KB; a unit
test in `codemode.rs` checks this with the default registry.

The default native instructions follow the `Execution environment:` line:
call independent tools together in one message, search exact identifiers before
reading, use `edit`/`write` for changes, and return the final answer as the last
message. A tool batch completes before the next model request; effectful calls
execute in message order. Read calls currently execute in order too. Native
`submit_result` calls settle after the other calls. A `write` followed by an
`exec` of that file therefore runs in one batch without another model request.

An ordinary tool failure, including a missing-file read or unknown tool, produces
its own `is_error` result and later native calls still run. Each result retains
its call ID. Cancellation or a storage failure can abort the remaining calls;
an unknown effect outcome is not automatically retried. In JS, await dependent
calls in order; a thrown JS error aborts that cell.

Start parallel command work with two `exec({background:true,...})` calls. Once
their handles are returned, call `wait` once per handle in the same next batch.
`wait` accepts one handle and is unchanged. See `tests/native_batch_exec.rs` for
scripted batch-ordering and error-continuation checks.

JS-mode instructions apply only to the opt-in wrapper. They ask for independent
reads and checks in one cell, `tools.edit` for changes, and `text(markdown)` for
the final answer. Its help and discovery use the same filtered native catalog.
