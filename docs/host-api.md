# Host API

Status: built 2026-10-29. Shared host dispatch, profiles and caller read scopes are implemented.

Every front end calls the method table in
[`bubblepaw_runner::host_api`](../crates/bubblepaw-runner/src/host_api/mod.rs).
The library owns authorization, parameter validation, session admission and host
effects. SQLite remains authoritative. Transport adapters own framing, response
presentation and live delivery; the web serving view remains disposable.

```rust,ignore
use bubblepaw_sdk::{dispatch, Profile};
let opened = dispatch(host.api(), Profile::Full, "session.open",
    serde_json::json!({"title":"Working session"})).await?;
let state = dispatch(host.api(), Profile::Full, "session.state",
    serde_json::json!({"session_id":opened["id"]})).await?;
```

## Caller read scope

`HostCtx.read_scope` is independent of the method profile and mutation workspace.
`ReadScope::Host` permits reads across the host's projects; `ReadScope::Workspace(path)`
limits session reads, lists and events to that workspace. Hidden-session visibility
still applies. Event visibility also respects the host's run ownership scope.
Read-scope checks use metadata only: they do not bind legacy sessions, restore
archives or write transactions. Capability reads retain their existing storage
behavior, including access to archived history.

HTTP uses `Host`, so the Desktop chat list can browse all host projects. A later
remote adapter will use the same scope. stdio uses `Workspace(its workspace)`.
The CLI uses `Host` for host-wide listing and `Workspace` for workspace commands.
An explicit CLI session target retains the target's existing workspace handling.
Mutations continue to enforce the execution workspace regardless of read scope.
Embedders can set the context's read scope explicitly; HTTP embedders can override
its default with `WebServices.read_scope`.

Workspace-filtered search and sidebar retain the existing bounded global query
and ordering, then filter its results. A page can therefore contain fewer rows
when other projects occupy the query window. Session-list filtering happens
before pagination.

REST adapters reject malformed request bodies with HTTP 400 before dispatch.
Handlers receive decoded typed parameters; request validation has no host effects.
The shared JSON-RPC adapter retains the protocol 0.1 error envelope and codes.

## Table and profiles

`Method` contains its wire name, minimum `Profile`, optional legacy feature flag
and erased handler. `dispatch(&HostCtx, Profile, name, Value)` finds that entry,
checks the caller profile, decodes parameters and invokes the shared handler.
Typed parameters and results live beside their capability implementation and are
re-exported through the SDK's `host_api` module.

Profiles are cumulative: `Read` permits reads, `Edit` also permits model runs,
session changes and approval/question answers, and `Full` also permits provider
and configuration changes, process/environment control and deletion. All local
adapters pass `Full`. A profile refusal uses JSON-RPC code `-32003` and HTTP 403
before a handler can perform effects. A later remote adapter can supply the paired
device's profile without introducing another host implementation.

| Method | Minimum profile |
|---|---|
| `initialize` | Read |
| `config.get` | Read |
| `config.set` | Full |
| `config.reload` | Full |
| `providers.list` | Read |
| `providers.key.get` | Read |
| `providers.key.set` | Full |
| `providers.key.remove` | Full |
| `providers.save` | Full |
| `providers.remove` | Full |
| `providers.test` | Full |
| `providers.reload` | Full |
| `roles.set_main` | Full |
| `mcp.servers.list` | Read |
| `mcp.auth.start` | Full |
| `mcp.auth.complete` | Full |
| `mcp.auth.sign_out` | Full |
| `mcp.headers.set` | Full |
| `remote.cloudflare.sign_out` | Full |
| `remote.cloudflared.info` | Read |
| `remote.cloudflared.set` | Full; local callers only |
| `remote.share.state` | Read |
| `workspace_trust.list` | Full; local callers only |
| `workspace_trust.trust` | Full; local callers only |
| `workspace_trust.revoke` | Full; local callers only |
| `session.open` | Edit |
| `session.list` | Read |
| `session.view` | Read |
| `session.sidebar` | Read |
| `session.rename` | Edit |
| `session.rewind` | Edit |
| `session.compact` | Edit |
| `session.delete` | Full |
| `session.archive` | Edit |
| `session.unarchive` | Edit |
| `session.search` | Read |
| `session.fork` | Edit |
| `session.state` | Read |
| `session.history` | Read |
| `session.import` | Full |
| `todo.list` | Read |
| `question.pending_child` | Read |
| `host.command` | Edit |
| `events.after` | Read |
| `context.view` | Read |
| `context.structure` | Read |
| `context.summary` | Read |
| `context.events` | Read |
| `history.range` | Read |
| `history.messages` | Read |
| `environment.inspect` | Read |
| `environment.control` | Full |
| `run.start` | Edit |
| `run.resume` | Edit |
| `run.steer` | Edit |
| `run.follow_up` | Edit |
| `run.cancel` | Edit |
| `run.wait` | Read |
| `child.cancel` | Edit |
| `process.background` | Full |
| `process.list` | Read |
| `approval.list` | Read |
| `approval.decide` | Edit |
| `question.answer` | Edit |
| `question.answer_child` | Edit |
| `attachments.add` | Edit |
| `attachments.read` | Read |
| `artifacts.list` | Read |
| `artifacts.read` | Read |
| `tools.list` | Read |
| `skills.list` | Read |
| `storage.stats` | Read |
| `shutdown` | Full |

`initialize` keeps protocol version `0.1`, its existing legacy feature flags and
input limits. Advertising is generated from the descriptors, including method
names, so an added entry is visible to callers. Existing stdio method names and
result shapes are preserved. New methods expose capabilities previously limited
to HTTP, the CLI or the SDK.

### Settings methods

**Workspace trust.** `workspace_trust.list {}` returns `{workspaces: [{path, trusted_at}], workspace}`
(`workspace` is the host's own `config.get` trust state, or null). `workspace_trust.trust {path}` records
the folder in the user's trust store and `workspace_trust.revoke {path}` removes it (`{removed}`); both
answer with `reload`, the report of the config reload they trigger, so a session opened afterwards applies
(or stops applying) that workspace's `.bubblepaw/config.toml`. A running session is not reloaded. `path`
must be an existing directory. All three need Full and refuse remote callers. When `session.open` creates
a root session while the host's project config is skipped as untrusted, the session gets a durable
`host_state_changed` event with key `workspace_trust` and value `{status: "untrusted", workspace,
config_file, sections, how_to_trust}`; it is also readable as the session's host state and replays through
`events.after`, so a client that opens the session can offer a Trust action ([workspace trust](workspace-trust.md)).

`config.get` also carries `workspace_trust` (status, workspace, config file, sections, how to trust) so a client can show a skipped project config ([workspace trust](workspace-trust.md)).

`config.get` returns host metadata and, from a host with an effective config, a
`settings` list (`{key, value, live, pending_restart}` per leaf, redacted like
`bubblepaw config`); the optional `key` selects one leaf or a section. `config.set`
(`Full`) changes one leaf key in the config file and applies it through the reload
path: `{key, applied: "live" | "restart_required", reload}`, with `value: null`
removing the key; on a map entry (`mcp.servers.<name>`, `providers.<name>`,
`toolsets.<name>`, `skills.<name>`) `null` removes the whole entry and a JSON object
replaces it in one validated write. Refusals are `-32010` unknown key, `-32011` wrong type, `-32012`
invalid value (file untouched) and `-32013` write failed. When the refusal is about one
leaf, the error's `data` is `{"key": "<full dotted key>"}`, such as
`mcp.servers.docs.url` for a bad URL in an object write; with no single culprit `data` is
omitted. Details and the validation
steps: [config](config.md#reading-and-changing-settings-over-the-host-api-2026-10-31).


### Provider keys (v0.13.0)

`providers.key.get {name}` (`Read`) returns `{is_set, source}`, where `source` is
`stored`, `environment`, or `none`. `providers.key.set {name, value}` and
`providers.key.remove {name}` require `Full`, including over a remote tunnel;
`Read` and `Edit` are refused before decoding a key. Writes return the same
status plus `applied` and `restart_needed`. A configured host rebuilds its live
provider generation, so the next request uses the new key and in-flight requests
retain their old generation. A setup without a live provider binding reports
`applied: false, restart_needed: true`.

The stored key overrides `key_env`; removing it restores the environment
fallback. Files are `<data_dir>/secrets/provider-<hex-encoded-provider-name>.key`,
with validated provider names and atomic writes. Like the Cloudflare token,
these are plain text protected by file permissions (`0600` on Unix, inherited
directory ACLs on Windows), not encryption or an OS keyring. No read returns
the value, and key-operation errors do not echo submitted values. Keys are not
stored in config, events or transcripts. See [configuration](config.md).

### Chat title search (v0.13.0)

`session.list` accepts optional `query`, a case-insensitive literal substring
of the chat title. Filtering precedes creation-order pagination and keeps the
existing hidden-session, archive and read-scope rules. Host-scoped callers search
all projects; a workspace-scoped caller remains limited to that workspace.
`query` alone enables the paged result (`sessions`, `next_cursor`), with the usual
default limit of 100 and maximum of 1000. An empty query matches all titles.
Without query, limit or cursor, the legacy array response stays unchanged.
`initialize.features` advertises `session.list.title_search`. The SDK's
`Client::search_session_titles(query, limit, cursor)` first reads that capability
on every call, then returns `sessions::SessionPage`; it rejects older hosts with
`ClientError::Protocol` because those hosts silently ignore the query parameter.

Root chats retain their separate SQLite files. Titles are read from the existing
central catalog's session metadata, without opening every root file. Only the
returned page is enriched with usage and runtime state; archived chats stay cold.
Workspace-scoped reads can inspect matching roots to resolve workspace bindings
before pagination; the host-wide title scan does not require that step. Reading
an archived result's workspace may temporarily decompress its archive without
restoring the canonical chat file. No search index was added.

Measured 2026-11-04 on linux01 with an unoptimized Rust test build: **10,000 real
active root SQLite chat files**, created through the normal store API, with
10,000 catalog rows. The verified root files total 3,358,720,000 bytes (3.36 GB).
These are empty chats with saved titles, without message histories. Fixture
creation (161.840 s), closing the initial store (21.977 s), and opening a fresh
host (33.299 s) are excluded from query timings. Host startup opens the active
root databases; its 33-second cost remains a preexisting scaling limitation.
The first query follows startup and file verification, so it is not a cold-disk
measurement.

Each host-wide title query reads the central catalog through
`SELECT data,last_activity FROM sessions` (`EXPLAIN QUERY PLAN`: `SCAN sessions`),
then performs Rust lowercase substring matching, with pending metadata overlays.
Returned rows retain ordinary usage/runtime enrichment. First hit: **124.583 ms**;
first miss: 58.115 ms. Across 20 warm repetitions, hit p50/p95:
**114.920/125.898 ms**; miss p50/p95: 57.997/62.009 ms. Both matching pages were
verified. The measured warm p95 under 126 ms does not justify a new title index;
the separate host-startup cost is outside this change. These timings exclude
SDK capability preflight, transport/model latency and populated-history result
enrichment.

The invocation raises only its own file-descriptor limit to 65,536: the ordinary
SSH process limit of 1,024 cannot cover the host's cached connections to 10,000
active roots. No system limit or configuration was changed. The fixture uses
automatic temporary-directory cleanup. Reproduce on Linux:

```sh
(ulimit -n 65536 && CARGO_BUILD_JOBS=2 timeout 900 cargo test -p bubblepaw-runner --lib title_search_10k_catalog_measurement -- --ignored --nocapture)
```

### Sharing settings (v0.13.0)

- `remote.cloudflare.sign_out {}` (`Full`) returns `{changed}` and removes only
  the browser-login `cert.pem` resolved by the existing Cloudflare login rules.
  It leaves the API token, named tunnel credentials and tunnel state intact.
- `remote.cloudflared.info {}` (`Read`) returns `{info, checked}`. Info retains
  `path`, `version`, `pinned`, `update_available` and `managed`, and adds `chosen`
  for an explicit executable selection. Remote callers never start a process:
  they receive the last matching cached validation result, or
  `{info: null, checked: false}` when the current executable has not been checked.
  Local reads validate the executable. Cached failures retain their clear error.
- `remote.cloudflared.set {path}` (`Full`, local only) stores a chosen path in
  `remote.cloudflared`; null clears it and restores managed-copy/PATH discovery.
  The host runs `--version` with a short timeout before accepting a path, refuses
  missing, non-executable or wrong programs, and leaves the executable in place.
  Restart sharing to run the newly selected executable. Remote devices, including
  `Full`, are refused with `-32003`, through both direct and attached sessions;
  `config.set` cannot bypass the restriction.
  Workspace `.bubblepaw/config.toml` cannot choose `remote.cloudflared` or
  `storage.data_dir`; both values are ignored with warnings, also preventing
  indirect redirection of managed binaries and stored provider keys.
  Before tunnel setup or launch, sharing uses
  the same validation, reusing results for matching executable contents and
  permissions. A rejection appears in sharing failure state with its reason.
- `remote.share.state {}` (`Read`) returns `status`, `address`, `lastError` and
  optional `failure: {transport, exit_code, retry_after_secs}`. An exit code is
  supplied when cloudflared exits with one. Retry seconds describe the actual
  next scheduled delay, including relay backoff, rather than a fixed 30-second
  promise. No active server gives null status/address/error/failure.

### Tool and skill catalogs

`tools.list` (`Read`) returns `{tools: [{name, category, description, source, server,
read_only}]}` for every published tool, whatever `agent.toolset` selects: `category` is
`files`, `commands`, `web`, `agents`, `history`, `other` or `mcp:<server>`, `source` is
`builtin` or `mcp`, `server` names the MCP server (else null) and `read_only` says
whether the built-in `read-only` toolset includes it. Built-in categories come first.
`skills.list` (`Read`) returns `{enabled, skills: [{name, description, source, path,
enabled}]}`: `enabled` is `skills.enabled`, `source` is `workspace` or `user`, `path` the
`SKILL.md` file and the per-skill `enabled` the `[skills.<name>]` switch. Both need a
host with live configuration and share the services error otherwise. Toolsets and skill
switches are changed with `config.set` ([config](config.md#toolsets-2026-10-31)).

### MCP server methods

MCP server settings change through `config.set` on `mcp.servers.<name>.*`, or write
a whole server with one object on `mcp.servers.<name>` (so a new server can start
disabled, and a server can switch between `command` and `url` without losing its
stored secrets). Leaf keys: (`enabled` turns a server on or off; `oauth`, `url`, `include_tools`,
`exclude_tools` and the rest are ordinary leaves). These methods cover only what
config cannot (details: [mcp](mcp.md#oauth-sign-in)):

- `mcp.servers.list {}` (Read) returns `{servers: [status]}`, one per configured
  server, disabled ones included.
- `mcp.auth.start {server}` (Full) returns `{url}`, the authorization URL for
  the client to open. Core opens no browser; it waits on a loopback redirect.
- `mcp.auth.complete {server, redirect_url}` (Full) completes the waiting
  sign-in with the callback URL the browser reached, and returns the status.
- `mcp.auth.sign_out {server}` (Full) forgets the tokens, cancels a waiting
  sign-in, and returns the status.
- `mcp.headers.set {server, header, value}` (Full) stores a literal header
  value for an HTTP server in its secret file (`<data dir>/auth/mcp-<server>.json`,
  never the config) and reconnects the server; it returns the status. `value`
  is required: a string stores it (replacing the same header name in any
  letter case), `null` removes it. The stored value is sent beside
  `headers_from` and wins for the same header. The header must be a valid,
  non-reserved HTTP header name; the value non-empty and at most 8 KiB, at most
  32 headers per server; `Authorization` is refused for an `oauth` server.
  Values are never returned or logged.
- Removing a server with `config.set {key: "mcp.servers.<name>", value: null}`
  also deletes its secret file. Replacing it with an object keeps the file; the
  OAuth tokens go only when the entry's `url` changes or it stops being a URL server.

A status is `{name, transport: "stdio" | "http", enabled, state: "running" |
"stopped" | "error" | "removed", error?, auth: {state, error?}, headers:
[name], tool_count, tools: [{name, enabled}]}`. `headers` lists the names (never
the values) set with `mcp.headers.set`. `auth.state` is `none`, `needs_sign_in`, `waiting`,
`signed_in`, `expired` or `failed`. `tools` lists the server's raw tool names,
with `enabled` false for ones `include_tools`/`exclude_tools` filter out;
`tool_count` counts registered tools. An unknown server, a server without
OAuth, or a failed step is an ordinary application error. Status, errors and
notifications never contain tokens or config values.

Each status change is pushed as an `mcp.server` notification whose params are
the whole status of one server (`state: "removed"` once it leaves the config).
After a lagging subscriber falls behind, every server's status is sent again.
stdio and attached clients get JSON-RPC notifications; the web adapter sends
SSE events named `mcp.server` on `/api/events`.

## Capability modules

The modules under `host_api/` separate sessions, runs, approvals, questions,
providers, context, processes, events, environments, attachments, forks, artifacts
and configuration, plus bounded session views and their SQLite cache. `HostCtx` contains the harness/store/workspace and optional
host services. Reduced hosts expose the same table and return a shared error
when a required service is unavailable. `HostCtx::for_store` creates an offline
context for store commands without starting providers or MCP servers.

`HostError` owns the application error's JSON-RPC code and HTTP status. A model
operation (`run.start`, `run.resume`, `session.compact`, and the slash commands
that call them) on a host whose main route has no usable provider fails with
`-32004` (HTTP 409, `no_provider_configured`). Reads, `providers.*`,
`roles.set_main` and `config.*` keep working in that state, so a client can
configure a provider on the running host; `providers.list` reports the reason in
`main_unavailable`. The message uses config keys and host methods, never CLI flags;
only `bubblepaw run` adds flag wording for its own users. The SDK surfaces the code as
`ClientError::NoProviderConfigured`. No other wire field changed. JSON-RPC
framing uses `rpc::handle` or `rpc::handle_bytes`: malformed JSON is `-32700`,
invalid request envelopes are `-32600`, and ordinary application errors retain
`-32000`. Notifications execute without a response. The HTTP adapter maps the
shared status to its existing REST error envelope.

## Adapters

- **stdio:** reads bounded JSON records, invokes the shared JSON-RPC handler and
  writes replies. Live events are notifications (`event`, `events.gap` and
  `mcp.server`); shutdown drains the host.
- **HTTP:** existing `/api/...` REST routes keep their original shapes and project
  the shared method result. `POST /rpc` accepts stdio JSON-RPC objects, including
  notifications. Existing local Host/Origin and body-size boundaries also apply
  to `/rpc`. `/api/events` remains SSE.
- **CLI:** session listing, history, search, import, archive/unarchive, delete,
  run/resume, cancellation and slash commands use the table. Presentation and
  host startup remain CLI responsibilities. `session.open` takes an optional `workspace`; a remote caller may name only a workspace the host knows (default or an existing session's), else `-32003` with `data.workspace`. Delete suppresses completion wakes
  and cancels owned processes. Cancellation resolves pending questions and
  cancels child work through the same control as the other front ends. Session
  listing includes usage, the session's `workspace` path (null when none) the `needs_you` (pending approval or question), `waiting_for` (`approval`, `question` or null) and `running` (turn running) flags, additively, in both the array and paged forms, under the caller's read scope. Catalog, quota, export, usage and serving maintenance
  retain their separate command entry points.
- **Remote:** `bubblepaw remote serve` serves paired devices through a relay or a Cloudflare Tunnel over
  an end-to-end encrypted tunnel. Each tunnel is a stdio-shaped session with the
  device's stored profile (`read`, `edit` or `full`) and `ReadScope::Host`; a
  method above it is refused with -32003. See [remote](remote.md#runner-remote-host).
- **SDK:** re-exports `dispatch`, `Method`, `Profile`, `ReadScope`, `HostCtx`, `HostError` and
  the typed capability API. `Host::api()` supplies the built host's context.
  Existing todo/question helpers call the same implementations.

Live delivery uses `events::subscribe`: subscribe first, capture the durable
boundary, then replay. Both transports use its replay/reset and visibility
rules. `events.after` is the ordinary paged table method. Stream fragments stay
transient and external effects with unknown outcomes are never replayed.

## Add a method

1. Add public serde parameter/result types and the implementation in the matching
   capability file. Keep new source files below 500 lines.
2. Add one descriptor with a stable wire name, minimum profile, optional legacy
   flag and handler. Keep authorization ahead of effects; a slash command must
   also enforce the profile of the operation it invokes.
3. Add a demo/scripted behavior test and a fixture in the table-driven transport
   parity test. An uncovered descriptor fails that test explicitly.
4. Add REST/CLI presentation only if needed, calling the descriptor through
   `dispatch`. Update this table and the applicable protocol contract.
5. Run focused remote tests, then the CI formatting, strict Clippy, full suite
   and file-size ratchet. No paid model is needed for these checks.

The transport guard rejects direct session control, admission, run-start,
approval-decision and question-tracker calls in transport source. It reports the
file and forbidden identifier. See [verification](verification.md) for acceptance
levels and [session API](session-api.md) / [web contract](web-contract.md) for the
wire contracts.

## Remote device management is outside the method table

Apart from the sharing settings methods above, Remote access settings (paired
devices, device key, saved hosts, Cloudflare domain setup, the serve lock) remain
outside the host method table. A remote client cannot do those operations to the
host it is talking to: they act on the local machine's files and
processes, or on the device's own key and host list, and the control endpoint
and the SDK are their only doors. They are listed here as exclusions from the
parity rule:

| Operation | Why it is not a method |
| --- | --- |
| Paired devices list and revoke | Edited by the host's owner on the host (`bubblepaw remote devices`, the control endpoint, `RemoteServer::devices` / `revoke`); a paired device must not manage others, and a revoked device cannot answer. |
| Device key info, export, import, regenerate | Local key material; never crosses a tunnel. |
| Saved hosts list, forget, probe | The device's own store, read before any tunnel exists. |
| Cloudflare login state, zones, `tunnel_login`, `cloudflared` update | Credentials and a program on the host machine. |
| `serve_status` | Answers "is this data directory already shared" before a server exists. |
