# Configuration

**Status:** built 2026-10-09; storage layout, import and cold-storage configuration added 2026-10-12; chat function calling, child session limits, `[exec]` and shared web cache configuration added 2026-10-13; stale-child reminders, background worker retention, completion wake and live config refresh added 2026-10-14; root worker instructions, rounds and task files added 2026-10-17; native default, persisted chat modes, JS opt-in and MCP core/deferred declaration policy added 2026-10-22; per-section settings types as the key list and live classification 2026-10-30; section owners on watch channels for model/provider, context and MCP 2026-10-31.

One TOML file replaces the growing set of CLI flags and built-in defaults for
models, providers, context strategy and approvals.

## Layers

Later layers win:

1. Built-in defaults.
2. User file: `~/.bubblepaw/config.toml`.
3. Project file: `.bubblepaw/config.toml` in the workspace.
4. Environment variables.
5. CLI arguments.

- **Custom file:** `--config <path>` or `BUBBLEPAW_CONFIG=<path>` replaces layers 2
  and 3, so a run (replay, tests, CI) depends on one known file.
- **Every key can be overridden**, with no hand-written flag needed per key:
  - Env: `BUBBLEPAW_` plus the key path in upper case, with `__` between levels,
    for example `BUBBLEPAW_CONTEXT__STRATEGY=classical` or
    `BUBBLEPAW_ROLES__SUMMARY='["opencode/deepseek-v4.1-flash"]'`.
  - CLI: repeatable `--set <key.path>=<value>`, for example
    `--set context.strategy=classical`.
  - Values parse as TOML (lists, tables, numbers, booleans). Anything that does
    not parse is a plain string.
  - Unknown keys are errors, not silently ignored.
- Common keys keep short flags (`--model`, `--decider`, `--summary-model`,
  `--context-strategy`, `--auto-approve`, ...). They are aliases for `--set`.
- `bubblepaw config` prints the effective merged config, naming the layer each
  value came from.

Credentials never appear in the file. A provider names the environment variable
that holds its key (`key_env`).

A host starts without a usable main provider (no key variable set, no readable
Codex auth file, or an empty `base_url` on the selected route). Configure one with
the `providers.*` and `roles.set_main` host methods, or by editing `model.provider`
and `providers.<id>`; until then model work fails with the typed error described in
[host API](host-api.md). `bubblepaw run` reports it with the matching flags.

### Account quota

Provider and extension entries accept optional `account = "name"`; omission
uses the entry name. Give aliases of the same login the same account. This
identity accompanies request-usage exports and shares native quota readings
across the data directory. It does not select credentials or route requests.
OpenCode routes (`opencode`, `opencode-*`) read Go subscription quota only after
`bubblepaw login opencode [--account <name>]`; the login is stored per account
under `<data dir>/auth/` and the default account is `[providers.opencode]`'s
`account`, or `opencode`. Give `opencode-messages` the same `account` to share
one login and one set of readings. See [quota](quota.md#opencode-console-login).

`[quota] max_wait_seconds = 21600` caps automatic waits for exhausted windows;
`0` declines them. This setting takes effect at host startup. HTTP account edits
follow provider-generation reload; extension account/env changes need restart.
Quota pollers are built once per host process from the first request's settings,
so a newly added account needs a host restart.
The host polls Claude subscription usage only on demand, using the configured
Claude Code credential directory without refreshing or writing tokens. See
[quota](quota.md) for sources, rejection classes, recovery and CLI commands.

### Execution mode

```toml
[agent]
execution_mode = "native"  # default for new chats; "js" opts into the wrapper
```

Native mode exposes core capabilities plus `tool_search` and `call_tool`, and
never declares `js`. JS mode declares only `js`, which wraps the same tool
dispatcher. `enable_js` is removed: either boolean value fails to load with a
message naming `execution_mode = "js"`.

Each chat stores the execution mode it starts with. Resume keeps that mode;
legacy chats without a stored mode use JS. New chats take the configuration
default. Selecting a mode does not register or authorize additional capabilities.
See [execution modes and embedder APIs](codemode.md).

The environment override is `BUBBLEPAW_AGENT__EXECUTION_MODE=js`; the CLI
equivalent is `--set agent.execution_mode=js`.

### Root worker instructions, rounds and task files

The root harness keeps its existing instructions and runs without a round ceiling
unless an explicit limit is configured. These settings use the same file, environment and `--set` layers:

```toml
[agent]
system_prompt = "Complete the assigned task. Return concise evidence and remaining issues."
# max_rounds = 48  # optional; omitted means unlimited
```

`agent.system_prompt` replaces the root's default instruction **body**, including
its built-in workflow and attachment guidance. The short execution-environment
prefix (OS, workspace and shell) remains so tool instructions match the host.
Root sessions also get a `Workspace access:` sentence from the live
`approvals.read_only` setting, appended after the body, so a read-only toggle
applies on the next request; child sessions keep their own static access text. The ordinary frozen repository memory and skill catalog are
still added when enabled; provider adapters may also add their own instructions.
Omitting the key retains the exact existing body; an empty string intentionally
omits that body. This does not replace the separate built-in child profile prompts.

`agent.max_rounds`, when supplied, must be positive; one round is allowed. It sets
the root harness default, not a token or dollar limit. Children inherit the
optional limit; there is no built-in twelve-round child ceiling. Library profiles
and per-run options may explicitly set a positive limit. Both root settings
take effect when the host starts and require restart after a config edit.
Use a fresh session to compare prompts; an existing compacted session retains
its frozen serving prefix and repository instructions.

For multiline tasks containing shell quotes, use a UTF-8 file:

```text
bubblepaw --model opencode/deepseek-v4.1-flash --json run --prompt-file "worker task.txt"
bubblepaw --set agent.max_rounds=48 --set 'agent.system_prompt="Inspect the assigned files."' run "Inspect this change"
```

`run` requires exactly one positional prompt or `--prompt-file PATH`. File paths
resolve from the invoking directory, and the file's text is read once without
shell interpolation. It follows the same prompt limits and slash-command
handling as positional text. Missing/unreadable/non-UTF-8 task files fail before
worker storage or providers are initialized. Existing `run "task"`, `--session`
continuation and `--no-session` temporary-store behavior remain available.
For example, environment overrides are `BUBBLEPAW_AGENT__SYSTEM_PROMPT` and
`BUBBLEPAW_AGENT__MAX_ROUNDS`; `bubblepaw config` shows their effective sources.

### MCP transports and authentication

MCP client servers use either a stdio `command` or a Streamable HTTP `url`.
The command remains a plain string; existing stdio configurations retain their
defaults. HTTP endpoints use HTTPS, except for `localhost`, `*.localhost`,
`127.0.0.0/8` and `[::1]`. URLs contain no credentials, query or fragment.

```toml
[mcp]
header_key_envs = []              # optional extra allowed environment identifiers

[mcp.servers.local]
command = "example-mcp-server"
args = ["stdio"]
env_from = { SERVICE_TOKEN = "SERVICE_API_KEY" }

[mcp.servers.remote]
url = "https://mcp.example.com/mcp"
headers_from = { Authorization = "EXAMPLE_MCP_TOKEN" }
approval = "confirm"
startup_timeout_secs = 30
call_timeout_secs = 120
```

`headers_from` maps an HTTP header name to the environment variable containing
its **complete header value**. For the example, `EXAMPLE_MCP_TOKEN` contains
`Bearer <credential>` in the process environment. The configuration contains only
the variable name; a literal token or `Bearer ...` value is rejected. Header
variable names have the identifier shape `[A-Za-z_][A-Za-z0-9_]*` (at most 128
characters) and end in `_API_KEY`, `_KEY` or `_TOKEN`, or appear in
`mcp.header_key_envs`. Every allowlist entry has that same identifier shape.
The allowlist defaults to empty and applies to MCP headers independently of web
provider setup.

HTTP servers cannot set stdio `args`, `cwd`, `env` or `env_from`, or a `command`.
Stdio servers cannot set `headers_from`. Header names follow HTTP token syntax;
case-insensitive duplicates, framing and connection headers, `Accept`,
`Content-Type`, `Last-Event-ID`, and all `Mcp-*` protocol headers are reserved.
The transport manages those headers itself. Invalid settings fail before any
credential lookup or connection, even when the server is disabled.

Ordinary TOML, environment and CLI layers apply, for example
`BUBBLEPAW_MCP__SERVERS__REMOTE__URL=https://mcp.example.com/mcp` or
`--set 'mcp.servers.remote.headers_from={Authorization="EXAMPLE_MCP_TOKEN"}'`.
Referenced credential variables are excluded from generic `BUBBLEPAW_*`
configuration overrides. `config show` prints the reference names and never
reads their values. Header values are resolved at connection time and never
stored as chat/tool content.

The client offers protocol `2026-07-28`, with supported `2025-11-25` and
`2025-06-18` negotiation fallbacks. This is client-side tools over Streamable
HTTP; there is no MCP server, OAuth flow or deprecated HTTP+SSE transport.
See [MCP lifecycle and tools](mcp.md).

### Web provider setup writes

Web setup writes providers and `[model] main` to the launch-time custom file
(`--config` or `BUBBLEPAW_CONFIG`), otherwise `~/.bubblepaw/config.toml`; it never
writes the workspace layer. Edits preserve comments and unrelated TOML and
replace the target through a temporary file in the same directory. Keys are
referenced only by environment-variable name; responses show only their presence.

Provider tables merge recursively, arrays replace, and empty tables clear
inherited tables. Omitting or emptying an optional scalar removes it from the
target table. If defaults would supply it again, the save fails with a specific
400 error; `inherited_fields` identifies fields supplied below the target. A later
project, environment or CLI layer masking the saved settings produces a 409.
Saving validates and prepares the effective provider configuration, writes the
target file and applies it to the running host immediately. Main selection applies
the same way. A failed apply preserves the active settings. Tests use non-generating
HTTP requests.

Startup, explicit reload, Save and Test (including drafts and saved providers being tested) require HTTPS
for non-loopback `base_url` hosts. HTTP is allowed for `localhost`, `*.localhost`,
`127.0.0.0/8` and `[::1]`. Base URLs cannot contain credentials, queries or fragments.
A violation returns 400 with
`base_url must use https unless it points at this machine (localhost/127.0.0.1/::1)`.

`key_env` must retain its identifier shape and end in `_API_KEY`, `_KEY` or `_TOKEN`,
or appear in `[web] provider_key_envs`, an optional array of variable names that
defaults to `[]`. Setup reads the effective array afresh for each request and
returns it as the non-secret `allowed_key_envs` field. Other names return 400 with
`key_env NAME is not an allowed key variable; names must end in _API_KEY, _KEY or _TOKEN, or be listed in [web] provider_key_envs`.
The setup API does not edit the allowlist; configure it in TOML. Startup and reload
share these provider URL and key-variable validation rules with web setup.

## Session names and retention (built 2026-10-15)

Fresh `run` and `script` commands accept `--name <title>`; use `session.rename`
or `/rename <title>` to change an existing session. Names appear in lists and
search hits. There are no model-generated titles. `--no-session` on either
command uses a private temporary store, removed on normal or error exit. It
selects that store before opening or migrating storage, including when
`--data-dir` points at an existing store. It conflicts with `--session`.

`[storage] archived_retention_days` is omitted by default, keeping archives
forever. Setting a nonnegative integer deletes chats whose actual archive date
is strictly older than that many days at host startup. Zero makes archives
eligible at the next startup. Active chats are never eligible, and legacy
archives without a recorded archive date are retained. Deletion logs chat IDs
and removes the whole chat tree, its blobs, checkpoints and spill files through
the existing deletion path. This setting requires a restart. See
[storage](storage-model.md#names-temporary-runs-and-archive-retention-2026-10-15).

`[approvals] hardline = false` disables the fixed exec blocklist. It defaults to
true, applies before configured deny rules and every approval mode, and changes
live. Children inherit the setting. The matcher is best-effort insurance over
command text, not a sandbox; see [approvals](approvals.md).

## Local llama.cpp server (built 2026-10-15)

An already running `llama-server` at `http://127.0.0.1:8080/v1` uses the existing
chat-completions kind. For a server started with `--alias local`, this is a complete
selection:

```toml
[model]
main = "llama/local"

[providers.llama]
kind = "chat-completions"
base_url = "http://127.0.0.1:8080/v1"
reasoning = {}
```

Omit `key_env` for a server without authentication; the adapter sends no
Authorization header. Keyless configuration is accepted only for loopback chat
endpoints. When the server requires a key, set `key_env` to its environment
variable as for other chat providers. The model suffix must match the server's
alias. No new provider kind or model-facing tool is involved. Scripted config
and local HTTP fixture tests cover this path; no live llama-server run is claimed.

## Live configuration (2026-10-14)

Web provider saves, deletes and main selection apply immediately. External
TOML edits apply at the next turn start, after a stat of each original config
file's mtime and size. There is no watcher. Use `tools.reload({server?})` inside
`js`, `/reload [server]`, `POST /api/config/reload` or stdio JSON-RPC
`config.reload {server?}` to apply immediately through the same path.
The optional server name forces that MCP server to restart even when unchanged.
Reload uses the original paths and layer precedence. Credential values resolve when configuration
is applied and never enter the returned payload.

The host validates and prepares a replacement before publishing it. A model
request already in flight completes against its old configuration; the next
request or tool round uses the replacement. Existing conversations, children
and background workers remain available. Explicit provider/model pins remain
pins: removing their provider or model produces an error on their next request
rather than silently changing their route. Original history and opaque reasoning
artifacts are preserved. Replay checks the adapter/model and any recorded
provider route/endpoint origin, rejecting a recorded origin from a different
endpoint. States without the route tag inherit the current route when
adapter/model match, preserving plain-upgrade compatibility.

Live fields are providers, `[model] main`/`provider`, named model roles (including
effort), approval modes, `approvals.read_only` and the approver prompt
`approvals.auto_prompt`, agent settings, exec settings, MCP servers and web
enable/exposure, backends, credentials by environment name, address policy, user
agent, cooldown and call budgets. The context controls `context.window_tokens`,
`context.compact_threshold_tokens` and `context.auto_summarize` also apply live.
Session model reporting, context-window lookup and compaction use the accepted
snapshot, and `config.get` refreshes its model, read-only and context metadata
from that snapshot. CLI overrides still take precedence after reload. All other
`context.*` keys, storage, extensions and `web.cache_ttl_secs`/`cache_max_bytes`
require a restart and retain `restart_needed`. Context diff notices identify
each changed restart-only key. Running children and exec jobs keep their admitted budgets,
timeouts and output limits; future calls and wakes use the new settings.

Invalid config leaves the previous configuration serving. Reload returns a
diff/error result, and each session gets one small durable generated
`[host notice]` on its next model request. Unchanged checks write nothing.
Changes have no config approval gate. See [live config](live-config.md) and
[MCP lifecycle](mcp.md) for server generations and removed-tool errors.

The provider payload retains `restart_needed` for nonprovider disk changes that
require a host restart; provider-only saves leave it false. Its
`reload_needed` says the effective disk provider configuration differs from the
active one. `active_main` is the current qualified route; `launched_main` retains
the startup value; `main` represents the effective disk setting. Clients can show the
active route and offer a reload action for unapplied disk changes.

### Settings sections

Each config section is one serde struct in `crates/bubblepaw-runner/src/config/sections.rs`
(the MCP section in `bubblepaw-mcp`, the context sections beside their code), declared
with `config_section!` from `bubblepaw_core::config_schema`. A field line carries the
key's type and its liveness, `LIVE` or `RESTART`, so the struct is the single source for:

- **The key list.** `Unknown config key` comes from walking the schema: fixed tables,
  maps with any non-empty name (`providers.<name>`, `mcp.servers.<name>`,
  `extensions.<name>`, `delegate.classes.<name>`) and free-form header tables.
- **Which keys are live.** `restart_needed` and `same_provider_config` diff two configs
  key by key and read each changed key's flag. `EffectiveConfig::changes_from` returns
  the changed keys with their flag; `EffectiveConfig::with_live_from(live)` is the
  startup config with only `live`'s live keys (and their provenance) applied, which is
  what the provider generation is built from.
- **The documented keys.** A test parses the `toml` blocks in this page and fails when
  a documented key is missing from the schema or a schema key is undocumented.

### Section owners (2026-10-31)

A live section has one owner that publishes it whole on a `tokio::sync::watch`
channel (`settings_hub.rs`); components subscribe to their own section and read
it when they act, so a key is live when its consumer subscribes:

- **Model/provider** (`model`, `providers`, `roles`): `LiveProviderConfig` owns the
  generation; `subscribe()` yields each whole generation, `snapshot()` the current one.
- **Context** (`context.window_tokens`, `compact_threshold_tokens`, `auto_summarize`):
  `SettingsHub::context()`; the summarizing context reads capacity, threshold and
  automatic preparation from it at each request.
- **MCP** (`mcp`): `SettingsHub::mcp()`. `McpSettings::validate` runs before
  publication, so an invalid server leaves the previous section and servers serving;
  `McpHost::follow` binds the host to the section and `McpHost::apply(force)`
  reconciles to its latest value.
- **Exec** (`exec`): `ProcessTools::follow(hub.exec())`; each call reads the section,
  and workspace views share the subscription.
- **Agents** (`agents`): `AgentTools::follow(hub.agents())`; limits are read from the
  section at use. The wake coordinator keeps `auto_wake` and the debounce in atomics,
  so `AgentTools::reconcile()` (no settings argument) copies them after a reload.
- **Approvals** (`approvals`), **web** (`web.*` live keys) and **skills** (`skills`):
  read at use through `SettingsHub::approvals()`, `web()` and `skills()`: the
  approval policy, read-only tool filter and reminders, web request settings, profile
  availability and the skill directory check.

`LiveConfig` pushes no settings into components. It publishes the generation, then
triggers the two reconcilers that must report back or re-derive state: `McpHost::apply`
(its diff and errors enter the reload report) and the tool registry republish
(`publish_tools`, which also re-registers agent tools and the web sources).

Sections are published together with each accepted generation. A startup-only key
that changes (storage, `context.strategy`, `context.lcm.*`, `agent.*`) is not
published; `ReloadReport.restart_keys` names it, and `restart_needed` is its
non-empty case. Approvals/tools, web, exec, agents and skills still reach their
components by direct calls from `LiveConfig`.

Adding a setting means adding one field line and, if it is new, a line in the Shape
block below.

## Shape

```toml
[model]
main = "codex/gpt-6.1-sol"
provider = "codex"                   # route prefix for an unqualified `main`
fallbacks = []                       # ordered `<provider>/<model>` routes tried after main

[storage]
data_dir = ""                       # empty: central store <local data home>/BubblePaw-Core/store
finished_event_retention_hours = 24
telemetry_retention_days = 30
archive_after_days = 30             # 0 disables idle whole-chat archiving
# archived_retention_days = 90      # omit: keep archived chats forever; startup-only
cold_tail_messages = 128            # leave this many newest rows uncompressed
cold_min_messages = 1024            # large when count OR body bytes reaches its threshold
cold_min_bytes = 8388608            # 8 MiB
cold_row_min_bytes = 256            # compress only when the encoded row is smaller

[quota]
max_wait_seconds = 21600             # 0 declines automatic waits; startup-only

[providers.opencode]
kind = "chat-completions"
base_url = "https://opencode.ai/zen/go/v1"
key_env = "OPENCODE_API_KEY"
# account = "opencode"               # quota identity; omitted: the entry name
# auth_file = "~/.codex/auth.json"   # codex kind: credential file instead of key_env
reasoning = { thinking = "enabled", reasoning_effort = "low" }

[[providers.opencode.models]]          # exact per-model settings
id = "deepseek-v4.1-flash"
reasoning_replay = "none"              # chat only: none | tool_loop | all
image_input = false                    # omit to use the provider capability

[providers.opencode-messages]
kind = "anthropic-messages"
base_url = "https://opencode.ai/zen/go/v1"
key_env = "OPENCODE_API_KEY"

[providers.openai]
key_env = "OPENAI_API_KEY"

[roles]
summary  = ["opencode/deepseek-v4.1-flash", "main"]
chapter  = "summary"
decider  = ["openai/gpt-6-luna", "llm/summary"]
approver = ["openai/gpt-6-luna"]

[context]
strategy = "lcm"                     # lcm | classical
memory_files = true                   # AGENTS.md + CLAUDE.md; restart-only switch
compact_threshold_tokens = 200000
# window_tokens = 400000            # omitted: the model's catalog window
auto_summarize = true                 # compact automatically at the threshold
retry_base_ms = 500                   # startup-only retry backoff base

[skills]
enabled = true                        # SKILL.md catalogs in the system prompt
dirs = []                             # extra skill collections after project and user skills

[context.lcm]
summary_input = "fork"               # fork main Chat prefix; episode explicitly uses compact source
episode_prompt = "v2"               # v2 | v3: v3 adds a gist line and an Actions: log
chapter_prompt = "v1"               # v1 | v2: v2 adds decisions and sections
decider_enabled = true              # false skips semantic chapter decisions; volume rollover remains available
source_target_tokens = 24000
source_max_tokens = 32000
summary_target_tokens = 2000
chapter_summary_max_tokens = 8000
chapter_description_tokens = 300
chapter_rollover_tokens = 0          # disabled; positive values explicitly opt into volume rollover
decision_context_tokens = 4000  # accumulated open-chapter decision input
tool_evidence_tokens = 0             # full episode tool evidence; positive values opt into clipping (fork unchanged)
raw_tail_tokens = 32000             # manual/emergency gathering compatibility; not an automatic apply cutoff
index_tokens = 2000
output_reserve_tokens = 16000
child_compact_threshold_tokens = 100000
# apply_target_tokens = 50000       # omitted: effective threshold; no raw-history refill

[context.classical]
verbatim_user_tokens = 20000

[approvals]
confirm = "auto"
hardline = true                      # fixed destructive-exec patterns; live setting
tools = { bash = "auto", write = "ask" }
deny = []                            # exact tool names that are always refused
read_only = false                    # live: expose only read-only workspace tools
# auto_prompt = "..."                # approver prompt template; unset uses the built-in default

[agents]                            # Unset caps mean no limit.
worker_retention_days = 7           # finished internal worker inspection window
worker_keep_all = false             # debug: preserve successful workers too
auto_wake = true                    # wake idle parents on visible child/background exec completion
wake_debounce_ms = 1500             # coalesce nearby completions into one reminder message
# max_depth = 3
# max_active_per_parent = 8
# max_active_total = 32
# max_retained = 64
stale_reminder_secs = 7200           # Running-child inactivity reminders; 0 disables.

[web]
enabled = false
provider_key_envs = []                 # extra credential variable names allowed at startup/reload/setup
search = ["deepseek", "ddg"]           # ordered list: deepseek | ddg | searxng:<url> | brave
fetch_backend = ["direct"]             # ordered list: direct | crawl4ai:<base-url>
allow_private = false                  # true permits loopback/private-network fetches
user_agent = "BubblePaw/0.1"
key_env = "BRAVE_API_KEY"
deepseek_key_env = "DEEPSEEK_API_KEY" # local worker example selects DEEPSEEK_MODEL_API_KEY
deepseek_base_url = "https://api.deepseek.com/v1/tools"
cache_ttl_secs = 86400               # 24 hours; 0 disables caching.
cache_max_bytes = 67108864           # 64 MiB cached response-data bound.
# max_searches = 100                 # Optional per-client cache-miss budgets.
# max_fetches = 200                  # Unset means unlimited; hits do not count.
cooldown_secs = 300                  # Skip a rate-limited or blocked backend this long.

[web.expose]
fetch = true
search = true

[extensions]                         # replaces the --extensions JSON file
```

## Child session limits

`[agents]` provides optional caps for the runner's [child sessions](child-sessions.md).
`max_depth`, `max_active_per_parent`, `max_active_total` and `max_retained` are all
unset by default, meaning no limit. Workspace child profiles receive agent tools
and can start children at any depth. Research stays restricted to web tools and
file reads. The caps are available against runaway recursion.

The root is depth zero. An explicit `max_depth` gives children agent tools while
their depth is below that limit; zero disables spawning. Active caps count running
turns, so an idle child releases its active slot and a new send must pass admission
again. The total is shared across the host's parent and child harnesses.

Retention bounds only process-local handles. Inactive handles can expire; their
saved child sessions remain discoverable by `sessionId` for status, wait, send and
cancel, and remain in no-argument status listings. Addressing an evicted session
reattaches a fresh handle when a slot is available. Cancelled, timed-out and failed
turns retain their result and accept a new follow-up. Parent cancellation includes
idle descendants without handles.
Explicit active and retention caps must be positive.

`stale_reminder_secs` defaults to 7,200 seconds (two hours); `0` disables it.
Only running children that the parent has not messaged, polled, waited for or
read during this interval trigger a reminder. Reminders repeat at most once
per interval per child; interaction resets the interval. Delivery happens before
the parent's next model request through the typed reminder queue, without waking
an idle parent. This setting adds reminders rather than default timeouts.

Per-turn `output_schema`, `max_tool_calls` and `timeout` are launch/follow-up API
arguments, not configuration defaults. Tool calls and timeout remain unlimited
unless explicitly requested or bounded by the calling parent's remaining turn
controls. See [child controls](child-sessions.md#structured-output-tool-budgets-and-timeout).

These keys use the ordinary file, environment and CLI layers. For example,
`BUBBLEPAW_AGENTS__MAX_DEPTH=2` or `--set agents.max_active_total=6` sets an optional
cap. Config output includes each configured value's source.
`BUBBLEPAW_AGENTS__STALE_REMINDER_SECS=60` or
`--set agents.stale_reminder_secs=0` changes the reminder interval or disables it.

### Completion wake

`agents.auto_wake` defaults to `true`. Visible child terminal results and background
exec completion wake an idle parent in long-lived web/stdio hosts. Hidden workers
never wake callers; their ordinary hidden children can wake that direct parent.
`agents.wake_debounce_ms` defaults to `1500`; zero disables the
delay. Child and exec completions arriving within that interval share one turn and
one durable generated reminder message. Busy parents retain next-request reminder
delivery. Turning off auto-wake preserves that delivery too.

Stop/cancel suppresses wake until accepted user text. Archived/closed sessions are
ineligible. There is no wake-chain cap or restart replay. Configure through ordinary
layers, for example `--set agents.auto_wake=false`,
`BUBBLEPAW_AGENTS__AUTO_WAKE=false`, or `--set agents.wake_debounce_ms=2000`.
See [completion lifecycle](child-sessions.md#completion-notices-and-idle-wake).

### LCM sizes and background gathering

`context.lcm` settings apply when the host starts. Gathering is driven by complete
source rounds, independently of the serving threshold, for root chats, native
children and hidden parallel workers. Chapters can change phase without another
human message. Summary lengths are token guidance; oversized outputs remain
stored. Default tool evidence retains the complete delivered canonical arguments
and results, status and original message/call IDs. A positive
`tool_evidence_tokens` explicitly opts episode-only input into bounded excerpts.

`context.lcm.decider_enabled` defaults to `true`. Set it to `false` to gather
episode summaries without calling the decider role or writing semantic decision
records. Chapters then close only when accumulated
episode-summary tokens reach a positive `chapter_rollover_tokens` size limit;
the default `0` disables size rollover and keeps the chapter open. Summary and
chapter models still run. When enabled, exhausted decider failures remain fatal
to that gather and leave its source coverage unchanged. Like other LCM settings,
the switch takes effect when the host starts. Examples:
`--set context.lcm.decider_enabled=false` or
`BUBBLEPAW_CONTEXT__LCM__DECIDER_ENABLED=false`.
Metered replay also skips decider price and credential preflight while disabled;
its summary and chapter routes still require their normal credentials.

`context.lcm.episode_prompt` selects the episode summary prompt: `v2` (default)
or `v3`. Both use the same length target and limit. `v3` adds a one-sentence
gist on line 2 and a required `Actions:` log immediately before `Open:`, one
action per line (`- <Verb> <target>[ · <change>][ · <outcome>][ [<call-id>]][ ! <kind>: <reason>]`,
with closed verb and kind sets, repo-relative paths and full IDs). The Actions
rows replace the prose record of what was done, so a v3 summary is about as long
as a v2 one. `bubblepaw_runner::episode_actions::parse_actions` parses that log
into rows: several ` · ` outcomes are kept joined, Rewrote/Modified/Updated read
as Edited, Wrote as Created and Executed as Ran, Listed as Read, and other malformed lines are
skipped. `parse_gist` takes the first prose line after the title (skipping headings and
lists, stopping at `Actions:`/`Open:`), without backticks, capped at 25 words. Chapter summaries, the store
and the wire format are the same under either value. Like other LCM settings, it
takes effect when the host starts: `--set context.lcm.episode_prompt=v3`. See the
[episode prompt v3 comparison](lcm-episode-v3-comparison.md).

`context.lcm.chapter_prompt` selects `v1` (default) or opt-in `v2` at host
start, using the usual file, environment and CLI layering. v2 adds grounded
decision lines and named, contiguous episode sections to closed chapter data;
malformed blocks produce a warning and the ordinary chapter memory is retained
without the new fields. The grammar and storage contract are in
[LCM chapter synthesis](lcm-contract.md#gather).

The approved [LCM contract](lcm-contract.md) treats
`child_compact_threshold_tokens` as a child physical context cap, separately from
the root. The response reserve is deducted exactly once: a 100k child cap with a
16k reserve permits 84k input. The library accepts `None` to inherit the root cap.
These apply caps constrain main serving requests. Full-prefix summary forks
independently reserve response space against `context.window_tokens` and the
candidate model's actual window; they do not inherit the smaller main apply cap.
The [LCM contract](lcm-contract.md) explains this distinction and the live evidence.
The standalone example uses a 200k root cap and 100k child cap. Provider capacity
can lower these caps. A whole tool group may exceed a source target.

Apply consumes ready complete coverage under pressure, waiting for needed memory
before publishing. It serves complete chapter/open-episode memory and the exact
uncovered raw suffix; title-only replacement is not accepted. The saved prefix
then stays frozen until the next apply. Legacy per-body/raw-tail allowances must
not silently omit prepared memory. The
LCM worker record in the project notes covers implementation and verification.

### Background worker retention

`agents.worker_retention_days` defaults to `7` and controls retention of failed,
cancelled, timed-out or interrupted internal workers. Running workers and
unresolved effects prevent cleanup. Successful workers awaiting delivery
acknowledgement remain available even when retention is zero.
Successful workers are deleted after explicit delivery acknowledgement, regardless
of that inspection window. `agents.worker_keep_all` defaults to `false`; setting
it to `true` disables worker deletion and retention cleanup for debugging.
These settings do not change visible child handle retention (`max_retained`).

Use `--set agents.worker_keep_all=true`,
`BUBBLEPAW_AGENTS__WORKER_KEEP_ALL=true`, or the ordinary TOML layer. See
[background workers](child-sessions.md#background-workers) for delivery semantics.

## Storage directory and offline commands

`--data-dir <directory>` is an alias for `storage.data_dir`. The directory holds
`catalog.db`, disposable `serving.db`, and `chats/<root-session-id>.db`. A root
chat and every child it creates share one chat file. A file-defined `data_dir`
resolves relative to its config file; env and CLI paths resolve
relative to the invocation directory. An empty `data_dir` selects the central
store `<local data home>/BubblePaw-Core/store`, where the local data home is
`LOCALAPPDATA`, then `XDG_DATA_HOME`, then `HOME/.local/share` (`HOME/Library/Application Support` on macOS). Every workspace
uses that one store; each root chat keeps its own workspace binding, and later
`run` and `delegate` processes [attach](host-attach.md) to the first process.
`--data-dir` and `data_dir` still select an isolated store for tests and benches.

`--db <file>` selects the sibling `<stem>.data` directory of a file path and
reads nothing from the file. `--db` and `--data-dir` cannot be combined.

Finished-run journal records are retained for 24 hours and telemetry for 30 days
by default. The two retention keys are nonnegative integers; `0` allows cleanup
as soon as a finished record is eligible. Startup and idle housekeeping prune
disposable data while unresolved effects remain in the chat record.

These commands run without registering providers or reading their credentials:

```sh
bubblepaw --data-dir ./state catalog rebuild
bubblepaw --data-dir ./state serving wipe
bubblepaw --data-dir ./state export <session-id>
bubblepaw --data-dir ./state export <session-id> --out history.jsonl
bubblepaw --data-dir ./state delete <session-id>
bubblepaw --data-dir ./state import codex transcript.jsonl --title "Imported Codex chat"
bubblepaw --data-dir ./state import claude-code transcript.jsonl
bubblepaw --data-dir ./state archive <chat-id>
bubblepaw --data-dir ./state unarchive <chat-id>
bubblepaw --data-dir ./state search '"exact phrase"' --chat <chat-id>
```

`archive` compresses the complete root chat and its children to
`chats/<root>.db.zst` after verifying it. Pending work or another active reader
keeps the chat active. `unarchive`, ordinary history access and a search hit
restore the SQLite file through the same opening boundary. Idle housekeeping
archives chats after 30 days without activity; `storage.archive_after_days=0`
disables that automatic policy. Export can read an archive using a temporary
unpacked file and leaves it archived.

Cold row compression starts at 1,024 messages or 8 MiB of stored message bodies.
It only touches rows covered by a compaction or summary boundary, absent from
the serving view, and older than the newest 128 messages. A 16 KiB dictionary
is trained from at most 1 MiB of eligible samples per chat file. Rows under
256 bytes and encodings that do not save space are left plain. All five new
storage keys use the normal TOML, environment and CLI layers.

Search accepts SQLite FTS5 query syntax. The contentless index stores terms and
row references in `serving.db`; snippets read their canonical messages. The CLI
finishes missing index pages before returning results. A wiped index rebuilds
in bounded pages, opening archived chats only when rebuilding their missing
terms or resolving a hit. Already indexed archives stay closed.

### Transcript import

Import reads JSONL line by line and commits bounded batches (128 messages or
2 MiB by default). It creates a new chat and discovers Claude's adjacent
`<session>/subagents/agent-*.jsonl` files and Codex sibling JSONL files whose
session metadata names this transcript as their ancestor. Repeat
`--child <file>` to provide additional child transcripts explicitly. It records source paths, SHA-256 hashes,
converter version, counts and normalized content hashes inside the chat file.
Malformed or unsupported content fails validation before creating a chat.

Original user, assistant and tool content is retained. Repeated request history,
progress events and injected harness wrappers are excluded with counts. Native
compaction summaries remain derived source records with `origin="import"`;
BubblePaw makes no model calls or new summaries. Every reasoning artifact is kept
byte-exact (converter `bubblepaw-transcript-v2`): Codex `reasoning` items with
`encrypted_content`, ids and summaries, and Claude `thinking` blocks with
`signature` and `redacted_thinking` with `data`, become assistant provider state
tagged with their origin (see [storage-model](storage-model.md#reasoning-artifacts)).
Nothing is dropped; `encrypted_fields_dropped` stays only as a v1 compatibility count.

Imported exports retain the usual message lines and add tagged
`kind="import_derived"` lines for source-derived records. The first message
includes `import_provenance`. The latest source summary plus its canonical tail
forms the initial imported serving history; original messages remain available
through history reads and export.

Export is a read-only operation on an existing layout, with one complete message
per JSONL line, blob content inline, and child session IDs linking to separately
exportable histories. Each line carries `session_id`, `parent_session_id`,
`root_id`, `child_session_ids`, and the complete `message` object. It does not
run cleanup.
Stored original attachment blobs appear once, on the first message line, in
`attachments`: each entry carries its ID, metadata, blob hash, and base64-encoded
original bytes. An ordinary session with no messages exports no lines; an
import with only source-derived records still exports provenance and those records.
An `--out` file must not already exist. Export writes only that requested output;
without `--out` it writes stdout. Catalog rebuilding and serving wiping preserve
canonical chat history and summaries. Deleting a root removes its chat file and
whole tree; deleting a child removes that subtree while retaining its root chat.
The stdio `session.open` method accepts `parent_session_id` (`parent_id` alias),
and `session.delete` accepts `session_id`.

### Chat reasoning replay (2026-10-13)

Chat completions stores every round's `reasoning_content` (and any other
opaque response fields) in provider state. By default, the current tool loop's
reasoning goes back to its compatible origin. Override that policy per exact
model ID under its provider:

```toml
[[providers.opencode.models]]
id = "kimi-k3"
reasoning_replay = "tool_loop"
```

- `none` (explicit): stored reasoning is omitted.
  Text-only assistant turns keep the historical `{role, content}` wire shape;
  tool-call turns still carry `tool_calls`.
- `tool_loop` (default, including omitted settings and models not listed):
  reasoning goes back only for assistant turns of the current,
  unfinished tool loop (since the last user task or final answer), including
  the assistant tool-call turns when sending their results. Use this for
  DeepSeek tool loops that require `reasoning_content` on those turns. Generated
  runtime host notices do not close the loop.
- `all`: reasoning goes back on every assistant turn, and length-stop
  continuations carry the partial reply's reasoning.

Only reasoning stored by the same chat adapter and model is sent. A value other
than `none` is rejected for non-chat providers, whose replay is fixed: Responses and Messages
send stored reasoning only to their origin provider and model
([providers](providers.md#reasoning-artifacts-2026-10-13)). Model IDs are values,
not table keys, because they may contain dots.

## Decisions (2026-10-09)

- **Strategies use roles, not models.** LCM uses `summary`, `chapter` and `decider`.
  Classical uses `summary`. Changing a role changes it for every strategy, which
  keeps strategy comparisons fair.
- **No per-strategy role overrides** for now. They can be added later as
  `[context.<strategy>] <role> = ...` if needed.
- **Only the strategy is fixed per session.** Models follow the current config.
  Each summary and decision already records the model that produced it.
- **The library takes typed settings** (`ModelRoles`, `ProviderEndpoints`, strategy).
  Reading, layering and validating the file belongs to the runner.
- Existing CLI flags remain as overrides of the matching keys.
- A role entry may name another role (`"summary"`) or the main model (`"main"`).
  Decision roles may name an LLM through `llm/<role or provider/model>`.

The decider list is an ordered fallback chain; the [OpenCode example](../examples/worker-opencode-go.toml) tries both Jev routes before falling back to Flash through the LLM adapter.

## Built (2026-10-09)

The runner owns reading, merging, validation and source tracking in
`crates/bubblepaw-runner/src/config.rs`. `bubblepaw config` prints valid TOML
with a source comment for every value, without opening SQLite, reading key
values or contacting providers. Missing implicit files are fine; a missing
custom file fails. Unknown keys fail in their original layer even if a later
layer would replace them. Tables merge by key, arrays replace, and an explicit
empty table clears that table. Role strings and lists normalize to `models`,
retaining independently configured repeats and thresholds.

Defaults remain the raw main model `gpt-6-luna`, with `model.provider = "codex"`
for bare names. Summary/chapter select `opencode/deepseek-v4.1-flash`, then main.
Decider and approver have no built-in model chain: set `roles.decider` and
`roles.approver` yourself (for example `["openai/gpt-6-luna", "llm/summary", "llm/main"]`).
A decision that needs a model with none configured fails at that point with an
error naming the key; decisions with nothing to choose between do not call it.
Decider leaves repeats/threshold unset, preserving selected-model Luna
calibration (1 / 0.6) and other-model calibration (3 / 0.8).
The approver makes one yes/no decision per call: `roles.approver.repeats` and
`roles.approver.threshold` are rejected with a message to remove the key. Its
prompt is `approvals.auto_prompt` (a template with `{user_request}`, `{tool}`,
`{description}`, `{arguments}`, `{working_directory}`; unknown `{...}` is kept),
read live at each gate; unset uses the built-in default ([approvals](approvals.md#auto-mode)).
Compaction remains
`min(window, compact_threshold_tokens) - output_reserve_tokens` (defaults: 16k reserve, 200k root physical cap; omitted window uses the cap); classical retains the default 20,000-token
user-message budget. Only strategy is persisted per session; explicit strategy
configuration must match a resumed session, while an unconfigured resume keeps
the saved strategy. New run, script, stdio, web and replay sessions use config.

Flags and `--set` apply in argument order. Role flags keep their legacy fallback
lists: `--summary-model X` equals `--set 'roles.summary=["X", "main"]'` (bare X
gets an `opencode/` prefix); `--decider X` / `--approver X` replace the first
candidate while retaining `llm/summary` and `llm/main`. `--auto-approve [X]` sets
`approvals.confirm=auto` and optionally the approver list. Per-tool modes win
over the global mode. The legacy JSON reasoning alias replaces the whole
reasoning object. `--extensions <json>` remains supported and loads
`[extensions.<id>]` with `command`, `args`, optional `cwd` and optional `env`.
`env` is a table of literal variables set on that instance's process on top of
the inherited environment; the host also sets `BUBBLEPAW_EXTENSION_ENV_KEYS` to
the comma-separated names it set. Values whose names contain `KEY`, `TOKEN`,
`SECRET` or `PASSWORD` are shown as `<redacted>` in `config show` output, but
they are still plain text in the file. One Claude Code instance per subscription
uses this for `CLAUDE_CONFIG_DIR`; see [providers](providers.md#claude-subscriptions). File-defined
`cwd`/`auth_file` paths resolve against their defining file; env/CLI paths use
the invocation directory.

Additional keys expose existing flags: `model.provider`,
`providers.<name>.auth_file` (a credential-file path, never credentials),
`context.window_tokens`, `context.auto_summarize`, `context.retry_base_ms`, and
`approvals.read_only`. Provider kinds are `codex`, `responses`,
`chat-completions`, `anthropic-messages`, `decisions` (alias `openai-decisions`),
`openrouter-decisions`, and `scripted`. Qualified main models select a configured
LLM provider or extension directly. Unconfigured auxiliary main transports are
not newly registered, preserving the no-file registry behavior.
Chat completions supports OpenAI-style function calling as a main transport,
including several streamed calls in a turn and tool-result history. Configure
a compatible DeepSeek, OpenRouter or opencode endpoint, then select its
provider/model address:

```toml
[model]
main = "opencode/deepseek-v4.1-flash"

[providers.opencode]
kind = "chat-completions"
base_url = "https://opencode.ai/zen/go/v1"
key_env = "OPENCODE_API_KEY"
reasoning = { thinking = "enabled", reasoning_effort = "low" }

[[providers.opencode.models]]
id = "deepseek-v4.1-flash"
reasoning_replay = "tool_loop"
```

Set the named key environment variable before starting the runner. The same
selection is available as `--model opencode/deepseek-v4.1-flash`. Chat sends
function definitions only when tools are offered; `tool_choice` and
`parallel_tool_calls` are omitted because the core request has no controls for
them. Text-only requests retain their existing wire bytes, including reasoning
replay, structured output and an explicitly configured role output limit.

`model.fallbacks` is an optional ordered array of qualified provider/model
addresses for the configured main model. The local worker example selects
`opencode-free/deepseek-v4.1-flash-free` as main and
`["opencode/deepseek-v4.1-flash"]` as fallback. Known failed-call usage is retained,
and the successful route sticks for that source session, including stateless
summary calls associated with it. Cancellation or already published text stops
fallback. Only declared routes are attempted. Provider/model changes reset
incompatible native replay state; changing endpoint does not share a prefix cache.

OpenCode Jev uses a separate typed decision transport:

```toml
[providers.opencode-decisions]
kind = "opencode-decisions"
base_url = "https://opencode.ai/inference/systemone/v1/systemone"
key_env = "OPENCODE_API_KEY"

[roles.decider]
models = ["opencode-decisions/jev-1.13-free", "opencode-decisions/jev-1.13"]
repeats = 1
threshold = 0.0
```

The session goes in `x-opencode-session`. Explicit role candidates are attempted
in order after the prior candidate exhausts its retry policy. Decision calibration
uses the successful model; samples from a failed model are not mixed into it.
See [decision providers](decision-providers.md) for the typed wire contract.

The built-in Messages address is `opencode-messages/claude-haiku-5-5`.
Select it for summaries and inherited chapter requests with
`--set 'roles.summary=["opencode-messages/claude-haiku-5-5"]'`.
Role defaults are unchanged. `anthropic-messages` aliases accept `base_url`
(including the `/v1` prefix) and `key_env`; for direct Anthropic, use
`https://api.anthropic.com/v1` and `ANTHROPIC_API_KEY` with an appropriate model.
An empty base URL disables registration. Messages thinking is fixed to adaptive
with low effort by default. Set role effort through `roles.<role>.reasoning_effort`;
nonempty provider `reasoning` overrides remain rejected for Messages.
`--model opencode-messages/claude-haiku-5-5` and the same `model.main` config value
now select the primary Messages transport without requiring Codex credentials.
Replay preserves recorded assistant turns, so selecting a primary model does
not generate replacements. Messages `main` references resolve normally; using
Haiku for replay summary/chapter/decider calls still requires the separate
replay metering allow-list and relay update. The current allow-list rejects
`llm/main` when `main` is Haiku, before replay creates its archive.

Each of the four roles (`summary`, `chapter`, `decider`, `approver`) has
`reasoning_effort`, default `low`; accepted values are `low`, `medium`, `high`,
`xhigh`, `max`. Reasoning stays on: `disabled`, `none` and unrecognized values
fail validation. References share model selection, not effort; chapter keeps
its own low default even if `chapter="summary"` and summary uses high.
File, environment and CLI layering follows the ordinary config precedence:

```toml
[roles.summary]
models = ["opencode-messages/claude-haiku-5-5"]
reasoning_effort = "high"

[roles.decider]
models = ["llm/summary"]
reasoning_effort = "low"
repeats = 3
```

The corresponding environment override is
`BUBBLEPAW_ROLES__SUMMARY__REASONING_EFFORT=high`; the CLI override is
`--set roles.summary.reasoning_effort=high`. `bubblepaw config` prints the
effective value and its source. Role effort wins over provider defaults and is
sent as Messages `output_config.effort`, Responses `reasoning.effort`, or chat
`reasoning_effort`. Calibrated Decisions APIs without an effort control ignore
that setting. Role defaults and repeats/threshold calibration are unchanged.
`roles.<role>.max_output_tokens` is optional and unset by default. When set, it is the
output limit of each call: Responses `max_output_tokens`, chat `max_tokens`, and Messages
`max_tokens` in place of the model table value. When unset, Responses and chat send no
limit and Messages sends its model maximum (128000 for `claude-haiku-5-5`). A length stop
is continued either way (see `providers.md`, Output length), so the key sets the size of
each call, not the size of the reply. Decision providers do not read it yet.

Replay plan model signatures now include role effort; plans pinned before this
field was added need regeneration before use with this version.

`BUBBLEPAW_CONFIG` and variables
named by provider `key_env` are exempt from generic overrides; their credential
values are never rendered.

[config.example.toml](config.example.toml) is a copyable default configuration.
Native [web tools](web.md) use the same layers under `web`: access defaults off,
search accepts the existing CLI backend syntax, and `expose` selects fetch/search,
both enabled by default. `--web`, `--web-search` and `--web-expose` override
those keys. The visible `research` profile uses the normal agents path and inherits
the active parent model and host Harness round limit. The old `research` tool,
`[web.research]`, `web.expose.research` and `--research-model` are removed.
`web.key_env` names the search credential environment variable;
config output never reads it. Invalid backends, URLs, empty lists and budgets fail even while
disabled. Brave is read only when listed; without a key it is skipped with a warning at search
time, not a configuration error.
`web.search` and `web.fetch_backend` are ordered backend lists (a single string still
works in files, environment and `--set`); see [web.md](web.md). `web.cooldown_secs`
(300; 0 disables) is the default skip for a rate-limited or blocked backend.
`web.allow_private` and `web.user_agent` control requests and partition cache keys.
`web.cache_ttl_secs` defaults to 86400 (24 hours); zero disables caching.
`web.cache_max_bytes` defaults to 67108864 (64 MiB) and bounds cached response
data through LRU eviction. The disposable SQLite file is `<data_dir>/web-cache.db`,
beside the catalog, shared across sessions and processes. It holds converted text,
passages and search replies, without raw HTML or authoritative source mappings.
Source IDs persist separately in the root chat and are shared by its children.
`web.max_searches` and `web.max_fetches` are unset/unlimited by default. Optional
positive values cap misses per shared client handle; cache hits do not count.
Zero call budgets are rejected. In Rust embedding configuration these fields are
`Option<usize>`; existing explicit numeric assignments become `Some(limit)`.
Verification uses loader tests, binary config checks, local transport fixtures
and the existing scripted replays. No live model acceptance is claimed.
The Messages follow-up adds effort layering, CLI primary routing and the
recorded-turn replay boundary checks alongside the provider fixtures.

## MCP servers

MCP client tools use stdio or Streamable HTTP servers configured under
`[mcp.servers.<name>]`. See [MCP transports and authentication](#mcp-transports-and-authentication)
for HTTP endpoints and env-only headers. The following example configures stdio.
The runner starts one shared server set when it builds a session's registry;
`config` and other commands that do not run sessions start no servers. Failed or
timed-out servers produce a warning and their tools are absent.

```toml
[mcp.servers.github]
command = "github-mcp-server"
args = ["stdio"]
cwd = "."                        # optional, relative to the workspace
env = { LOG_LEVEL = "warn" }     # plain values only
env_from = { GITHUB_PERSONAL_ACCESS_TOKEN = "GITHUB_TOKEN" }
enabled = true
defer = true                    # default; false declares this server's tools in native mode
approval = "confirm"             # default; "none" opts out
startup_timeout_secs = 30
call_timeout_secs = 120
include_tools = ["get_*"]        # omitted allows all; [] allows none
exclude_tools = []

[mcp.servers.github.tools]
get_issue = { approval = "none" }
```

`env_from` maps child environment names to host environment names. Values are
read only when spawning the child; `config` prints only the names. Children
inherit the host environment, with `env` and then `env_from` applied. Generic
overrides work here too, for example
`BUBBLEPAW_MCP__SERVERS__GITHUB__CALL_TIMEOUT_SECS=60` or
`--set mcp.servers.github.enabled=false`.
Generic environment paths are lowercased by the loader. To preserve the case of
child variable names, override `...__ENV` or `...__ENV_FROM` with a whole TOML
table, for example
`BUBBLEPAW_MCP__SERVERS__GITHUB__ENV_FROM='{ GITHUB_PERSONAL_ACCESS_TOKEN = "GITHUB_TOKEN" }'`.
Config files and `--set` preserve those map keys directly.

MCP tools are deferred by default in native mode: find their schemas with
`tool_search({query:"github"})`, then invoke them with
`call_tool({name:"mcp__github__get_issue", arguments:{...}})`. Set `defer = false`
to declare all included tools from that server as core tools instead. Changing
this flag does not restart its MCP connection. During a native run, reloads
update callable tools and append a harness notice; declarations and deferred
server anchors rebuild only at a new run or compaction apply.

Tools are also available inside `js` as `tools.mcp__github__get_issue(...)`, and
listed by `tools.$list()` / `tools.$search()` after run policy filtering.
Names are sanitized and capped at
64 characters; long names have a hash suffix. Every call is a durable effect,
and defaults to confirmation. `[approvals.tools]` uses the registered name;
session rules and those per-tool approval policies keep their usual precedence.
One-shot `run` denies unanswered confirmations; `--approve-all` allows them.
Timeouts return completed errors saying the outcome is unknown, never replayed.
See [mcp](mcp.md) for protocol, restart and process-lifecycle limits.

## Exec

Command execution settings live under `[exec]` (see [exec](exec.md)):

```toml
[exec]
sync_timeout_secs = 300      # sync tools.exec moves to background after this; 0 = background at once
# max_jobs_per_session = 4   # optional cap on live jobs per session; unset = no limit
# max_jobs = 16              # optional host-wide cap; unset = no limit
max_output_bytes = 8388608   # per-job output spool cap; head and tail are kept
```

- A sync call that reaches `sync_timeout_secs` returns `status:"backgrounded"` and the command
  keeps running; the per-call `timeoutMs` is still a hard kill deadline.
- The job caps are unset by default; set them only to bound a host. Exceeding a set cap is a
  correctable error before the command starts.
- The spool lives under `<data_dir>/jobs/` and is disposable; it is never part of the chat file.
- The usual layers apply, for example `BUBBLEPAW_EXEC__SYNC_TIMEOUT_SECS=600` or
  `--set exec.max_jobs=16`.

## Delegate

`bubblepaw delegate` (see [delegation-bridge](delegation-bridge.md)) has one config
section: the model classes an assignment's `route = { class = "..." }` can name.

```toml
[delegate.classes]
small = { default = "opencode/deepseek-v4.1-flash", candidates = ["opencode-messages/claude-haiku-5-5"] }
```

- Each class has a `default` model and optional other `candidates`, all as `provider/model`.
- Class names are free; an assignment naming an unconfigured class is rejected before anything runs.
- There are no other delegate keys. The explore share (0.15), the category list, the rework default (1),
  the check output tail (2048 bytes) and the price table (embedded, dated) are constants in the runner.
- The usual layers apply, for example
  `--set 'delegate.classes.small={ default = "opencode/deepseek-v4.1-flash" }'`.
