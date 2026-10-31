# Live config refresh

Status: Built through 2026-10-31 (section owners on watch channels, see [config](config.md#section-owners-2026-10-31)). Live session model and compaction controls; hooks and plugins stay deferred
([decisions](decisions.md)).

The approved refresh path is implemented in the runner: turn-start file stats,
one shared explicit reload path, live policy/settings, MCP generations and tool
list notifications, registry snapshots and bounded one-time durable diff notices.
Acceptance uses scripted providers and local fake MCP transports; it makes no
live model or external-server claim. See [verification](verification.md).

## Tail reminders (2026-10-21)

Model-facing notices queue through `ContextState::remind` and use
`<harness kind="KIND">...</harness>`. The next request flushes one combined
Generated user message after the latest input or tool results. Sent messages
stay in history unchanged; repeated snapshot text is suppressed across requests
and restarts. The queue itself never starts a model call.

Config, memory-file and skill diffs are incremental events with distinct kinds,
so multiple changes before one request remain visible. Runtime inventory is a
stable kind containing live facts only. Child/process completion and question
recovery use persisted kind and delivery identity metadata; legacy prefix
matching is limited to records predating migration. See the
[queue and storage contract](prefix-cache-design.md#tail-reminders-p2-2026-10-21).

A session's config-notice cursor starts at the current end of the host's notice
list when the session is first seen, including an explicit `session.open` before
its first model turn. Reopening an already observed session keeps its cursor.
It receives later reloads once, rather than replaying reloads that preceded it. Applied compaction rebinds the skill catalog
and memory-file prefix from current state and drops covered change notices; see
[skills](skills.md) and [memory files](memory-files.md).

## Intent

Other agent hosts need a restart to pick up config changes. BubblePaw applies
them inside a running session: a config edit by the user or by an agent takes
effect on the next turn, or right away through a `reload` tool. This covers the
everyday settings an agent might change mid-session: MCP servers, providers and
roles, web backends, approval modes, agent settings and exec settings.

BubblePaw is internal-only with no sandbox for now. Config changes have no
approval gate or access control, whether an agent or the user makes them.
Permissions and auto mode are a separate, later topic. Keep it simple.

## Baseline before this build (verified on main at 67123e8)

- **MCP client.** `crates/bubblepaw-mcp` speaks stdio and Streamable HTTP
  ([mcp](mcp.md)).
  - Servers start once per runner process (`main.rs:1008`) and are registered
    as host tools named `mcp__<server>__<tool>` (`main.rs:1012-1016`).
  - A dropped server reconnects lazily on the next call, once, and never widens
    its tool set (`host.rs:250-290`, `352-353`).
  - `notifications/tools/list_changed` is not handled
    (`protocol.rs:269-271`, `http.rs:561-567`).
- **Config.** Layered TOML is loaded at startup (`config.rs:188`).
  - Providers, roles and web backends already apply live through
    `config.reload` and `POST /api/config/reload`, with no file watcher.
  - MCP, approval, agents, exec and extensions are restart-only and set
    `restart_needed` ([config](config.md)).
- **Tool registry.** `ToolRegistry` is immutable after construction
  (`lib.rs:652`), owned by the `Harness`, and snapshotted per turn
  (`runtime.rs:925-934`).
  - Definitions are built per model round from a `BTreeMap`, so they come out
    sorted (`runtime.rs:1048`).
  - The model sees only `js`, whose description lists every deferred host tool
    name (`codemode.rs:975-1068`, `definition_in` at `codemode.rs:1194-1201`).
    The snapshot changes only at run start or after a model-initiated reload, so
    the description is stable within a run. So an unchanged tool set keeps the prompt
    cache, and a changed set breaks it once.
- **Claude Code provider.** It rebuilds its query when the `js` description
  changes (`extensions/claude-code/src/mirror.ts:168`).
- **One-time notices.** `Tool::take_reminders` returns structured reminders
  that the runtime queues for the next model request's tail.
- **Children.** A child snapshots the parent's tool registry when it starts
  (`main.rs:1225`).

## Design

### Check at turn start

At the start of each turn, the host stats every config file. It compares mtime
and size with the last check; there is no file watcher.

The same check also covers [repository memory files](memory-files.md), including
absent candidate paths. Their session system prefix stays frozen. Added, changed
or removed files deliver their path and bounded new content once through the
ordinary durable reminder path; unchanged files add no transactions or WAL.
Each session, including children, freezes its own initial snapshot. The
`context.memory_files` switch defaults to true and remains restart-only.

If anything changed, it re-reads and validates the whole config the way startup
does. An invalid config changes nothing, and its error goes into the note to
the model.

Every live section is applied from the new config:

- **Providers, roles and web:** the existing live apply. Session model reporting and
  context-window lookup use the current model.
- **Context controls:** `context.window_tokens`, `context.compact_threshold_tokens`
  and `context.auto_summarize` apply to subsequent context preparation. Other
  `context.*` keys remain restart-only, including strategy, memory-file enablement,
  retry timing, classical settings and LCM settings.
- **Approval modes and deny rules:** apply to the next tool call. Configured
  denies precede session allowances and every mode, including a pinned per-run
  override. Children union parent denies with their own policy.
- **Agent settings** (auto-wake, debounce, child budgets and timeouts): apply to
  new children and new wakes. Running children keep the budgets they were
  started with.
- **Exec settings:** apply to the next exec call. Running jobs keep their
  settings.
- **MCP:** servers are diffed by name. An added server is started, a removed
  one is stopped, and a server whose connection settings changed is restarted.
  Filter-only changes rebuild the inventory on its existing connection and
  count as changes even when the exposed names happen to remain identical.
  `include_tools` limits raw server tool names when set (`[]` hides all);
  `exclude_tools` removes names. Both accept exact names or `*` globs. The same
  filters apply to registration and `list_changed` refreshes. Other servers
  keep running.

Storage, extensions, web cache settings and the remaining context settings stay
restart-only. Context diff notices name each changed frozen key and mark it
`restart needed`; changes to the three live context controls do not set that flag.
`config.get` reads the accepted live snapshot for model, provider, read-only
status and the live context controls. CLI overrides retain their original layer
precedence on reload.

The explicit `config.reload` and the HTTP route run the same check immediately.

### MCP without breaking calls

A reload never interrupts a tool call. Servers are host-wide and shared by
every session, so each server connection is a generation:

- a call holds the generation it started on;
- a stopped or restarted generation closes only after its last call finishes;
- new calls go to the new generation.

The client also accepts `notifications/tools/list_changed` from a server and
refetches that server's tool list. The rule that a reconnect never widens the
tool set is replaced by "the server's current list wins, and the note announces
it".

### Registry swap

The registry moves behind a shared snapshot that is replaced on change. A turn
takes the snapshot at its start, and a round re-takes it only after a reload in
that turn. Children take their snapshot from the live registry at each of their
own turn starts. Core gains only this swap seam; config and MCP logic stay in
the runner.

### `reload` tool and `/reload`

`reload` runs the check immediately and returns what changed. It takes effect
from the next model round of the same turn. `reload <server>` restarts one
stuck MCP server even when its config is unchanged. `/reload` from the user
does the same thing.

### Note to the model

When something the model can see changes, each session gets one durable host
reminder on its next model request, through the typed reminder queue. The reminder holds only
the diff. For example:

`Config changed: + mcp__notes__import_markdown, − mcp__foo__* (server foo stopped: config removed), ! bar failed to start (auth error); approval for exec: ask → allow`

A call to a removed tool gets a clear error naming the reason.

The tool list stays sorted, so an unchanged set leaves the prompt cache intact.
A real tool change rewrites the `js` description and breaks the cache once;
changes are rare, so this is accepted.

A republish (reload or MCP `refresh_tools`) stamps the published registry with
the next generation (`publish_tools`). A run whose snapshot carries a different
generation from the session's previous run declares the break as
`tools_reloaded`, so its first request is a declared `frozen_change`. The
declaration comes from the republish itself, not from diffing definitions;
other tool-set changes stay undeclared.

## Build

One step:

- turn-start check;
- live approval, agents and exec;
- MCP diff with generations and `list_changed`;
- registry swap;
- the `reload` tool and `/reload`;
- the diff note.

It is done when scripted tests cover:

- each section changing mid-session;
- MCP add, remove and change;
- `list_changed`;
- a stuck-server reload;
- a call to a removed tool;
- an invalid config leaving the old one serving;
- an in-flight MCP call surviving another session's reload.

Update [mcp](mcp.md) and [config](config.md), moving the newly live sections
into the live list.

## Later

Hooks, then plugins in Claude Code's layout, using the same turn-start check.
Both are deferred.
