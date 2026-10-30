# MCP client tools

Status: built, implementation updated 2026-10-15; OAuth for HTTP servers proposed 2026-10-21.

Implementation updated 2026-10-15. BubblePaw connects to external MCP servers over stdio
or Streamable HTTP. It exposes no MCP server.

Tools appear in the registry, pass through [approvals](approvals.md), and are
recorded as durable effects. The runner exposes them inside `js`, including
workspace profiles; `tools.$list()` lists names and `tools.$search()` returns
definitions. The implementation is in [bubblepaw-mcp](../crates/bubblepaw-mcp/src/lib.rs).
Core and SQLite schemas are unchanged.

## Scope and protocol revisions

The client supports tools discovery and calls. Resources, prompts, sampling,
elicitation, roots, subscriptions, and multi round-trip input
requests are outside this host's scope. An unsupported input request or result
type produces an error that says the outcome is unknown; the call is not replayed.
The deprecated 2024-11-05 HTTP+SSE transport is not supported.

Streamable HTTP primarily uses
[2026-07-28](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http):
each JSON-RPC message is a POST to one configured endpoint, with
`Accept: application/json, text/event-stream`. Every request includes protocol
version and client capabilities in `params._meta`, along with client identity.
`MCP-Protocol-Version`, `Mcp-Method`, and the call's `Mcp-Name` mirror the body.
Responses may be JSON or SSE. `notifications/tools/list_changed` marks the
server inventory for refresh; other notifications and SSE comments are transient.
There is no initialization handshake, protocol session, GET stream, or DELETE
on the modern path.

Discovery first sends a modern, read-only `tools/list` request. The client follows
the spec's [backward-compatibility rules](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http#backward-compatibility):
a recognized `UnsupportedProtocolVersion` error selects an explicitly advertised
compatible revision, including an earlier revision that needs initialization.
An incompatible version list, `HeaderMismatch`, or
`MissingRequiredClientCapability` produces an actionable error. An HTTP 400
without a recognized modern error selects the earlier initialization path.
An unreadable or oversized error body is an unknown transport result and does
not trigger fallback. Authentication failures do not trigger fallback.

The earlier path offers `2025-11-25`, or the compatible revision explicitly
advertised by the server, and accepts `2025-11-25` or `2025-06-18`.
It performs `initialize`, checks tools capability, and sends
`notifications/initialized`. A returned `Mcp-Session-Id` accompanies subsequent
POST, GET, and DELETE requests, and `MCP-Protocol-Version` carries the negotiated
version. Initialization and its response-stream resume omit the version header
until negotiation completes. On a confirmed stale-session HTTP 404 rejection the client reinitializes
once, without the old session ID. An ambiguous interrupted call is never retried.
See the [earlier transport](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports).

Stdio keeps its existing newline framing and initialization: it offers
`2025-06-18` and accepts `2025-06-18`, `2025-03-26`, or `2024-11-05`.

## Configuration

```toml
[mcp]
header_key_envs = []                # optional explicit allowlist of env names

[mcp.servers.local]
command = "github-mcp-server"
args = ["stdio"]
cwd = "."                          # optional, relative to the workspace
# Plain process settings, not credentials:
env = { LOG_LEVEL = "warn" }
env_from = { GITHUB_PERSONAL_ACCESS_TOKEN = "GITHUB_TOKEN" }

[mcp.servers.remote]
url = "https://mcp.example.com/mcp"  # use url instead of command
headers_from = { Authorization = "MCP_API_TOKEN" }
enabled = true
approval = "confirm"               # default; or "none"
startup_timeout_secs = 30
call_timeout_secs = 120
include_tools = ["get_*", "search"] # optional allowlist of raw server tool names
exclude_tools = ["*_private"]      # exclusions win

[mcp.servers.remote.tools]
get_issue = { approval = "none" }
```

`headers_from` values are environment-variable names, never literal headers.
The environment value supplies the entire header, including `Bearer ` for bearer
authentication. Names must be identifiers ending in `_API_KEY`, `_KEY`, or `_TOKEN`,
or appear in `[mcp] header_key_envs`. The config and `config show` contain only
names. Values are resolved only when the host connects and are redacted from
server responses and diagnostics. Protocol and framing headers cannot be
configured, nor can headers duplicate each other by case.

HTTP endpoints require HTTPS except for this machine (`localhost`, `*.localhost`,
`127.0.0.0/8`, and `[::1]`). Credentials, queries, and fragments in URLs are
rejected. HTTP entries cannot also specify command, args, cwd, env, or env_from.
HTTP redirects, automatic request retries, and inherited proxies are disabled.
OAuth is not implemented: authentication rejection reports that OAuth is
unsupported and that preconfigured env credentials are required.

Stdio children inherit the host environment plus `env` and `env_from`. Credentials
belong in `env_from`, and its values are read only at spawn time. Generic layered
overrides use `BUBBLEPAW_MCP__SERVERS__<NAME>__...`; see [config](config.md).

### Request-scoped client environment

`McpHost::start_with_client_env(settings, workspace, McpClientEnv)` starts a host
whose credentials come from a client snapshot instead of this process. `start`
keeps the standalone behaviour above. With a snapshot:

- `headers_from` and `env_from` resolve only from the snapshot. A missing name is
  an error and never falls back to the host environment. The error text is the
  standalone text: "required MCP HTTP credential environment variable is missing"
  for headers and "required host environment variable is missing" for `env_from`.
- Names follow the platform rule: Windows names are case-insensitive, as in
  `std::env`, so `path` finds `PATH`. A later name that differs only in case
  replaces the earlier one. Other platforms compare exactly.
- A stdio child inherits the host environment, then config `env`, then config
  `env_from` (the existing precedence). Only the snapshot values that server's own
  `env_from` names reach it; another server's forwarded value never does, and an
  HTTP server sends only its own `headers_from` values. The runner selects which
  client variables enter the snapshot and does not pass provider-only secrets.
- Snapshot values are secrets: they are redacted from errors, stderr diagnostics
  and tool results. `McpClientEnv` prints only its names in Debug and is never
  serialized, so values stay out of settings, `config show` and traces.
- Each `McpHost` instance keeps its snapshot for startup, reload, refresh and
  reconnect. The runner creates one instance per request, so different sessions
  never share its processes, connections or client snapshot.

`[mcp] header_key_envs` is a validation allowlist, not a forwarding list. It admits
header credential names that do not end in `_API_KEY`, `_KEY` or `_TOKEN`. The
runner forwards only names referenced by the client's resolved MCP configuration;
it never forwards the whole client environment. See [host attachment](host-attach.md).

Filters match the server's original tool names before registration sanitizes or
prefixes them. Matching is case-sensitive, covers the entire name, and supports
`*` for any sequence of characters; other characters are literal. An omitted
`include_tools` permits all tools, while `include_tools = []` permits none.
`exclude_tools` removes matches after inclusion. Filters apply at discovery,
`tools/list_changed` refresh, reconnect inventory publication and live reload.

## Tools and results

Registered names are `mcp__<server>__<tool>`, sanitized to `[A-Za-z0-9_-]` and
capped at 64 characters with a stable hash suffix. Collisions and invalid schemas
are skipped with a warning. Descriptions and `inputSchema` come from `tools/list`.
Modern HTTP validates `x-mcp-header` annotations: only statically reachable
primitive properties are mirrored to `Mcp-Param-*`, using the required Base64
sentinel encoding for unsafe values. Invalid annotations skip the tool.

All tools have `read_only = false`, regardless of server hints. Approval defaults
to `confirm`, with server and per-tool overrides; session rules and
`[approvals.tools]` still take precedence.

Text content is joined. Binary and embedded resource blocks become size/type
placeholders. `structuredContent` supplies JSON text when there are no text
blocks. `isError` and JSON-RPC errors become error results.

## Connections, interruption, and shutdown

`McpHost` owns one server connection per configured entry, shared by all its
sessions. Discovery runs in parallel under per-server startup deadlines. Failed
servers contribute a warning and no tools. Non-session runner commands such as
`config` neither connect nor spawn children. The inventory is live and sorted
by registered name. The runner checks config at turn boundaries and through reload; see [live config](live-config.md).

`McpHost::reload(settings, forced_server)` validates settings before changing
anything, then diffs enabled entries by configured name. Added entries start,
removed or disabled entries stop, and changed entries restart. Unchanged entries
keep their connection. Naming a forced server restarts it even if its settings
are unchanged. Discovery of replacements is parallel and retains per-server
startup deadlines. A replacement that fails contributes a warning and no tools;
other servers continue serving. Invalid settings leave the old inventory intact.
Changing only filters retains the connection and rebuilds from its cached raw
tool list, so it can reveal previously filtered tools without rediscovery.
`McpDiff.filtered` names those servers even when their visible tools stay the same.

A server connection is a generation. Each invocation pins its generation before
connecting or sending. Replacement swaps the current inventory immediately after
discovery, while an owned cleanup task waits for the old generation's last call
to finish before closing it. New calls use the replacement generation, including
calls through retained tool handles whose definition is unchanged. A retained
handle for a removed tool returns its removal reason without sending a request.
If its schema, description, or approval policy changed, the old handle rejects
the call and asks for a fresh registry snapshot. This preserves the validation
and approval contract under which the call was prepared.

Both transports recognize `notifications/tools/list_changed`. Stdio reads these
on its connection; HTTP reads them in response SSE streams on both protocol
paths. No idle HTTP GET stream is introduced. Hosts call
`McpHost::refresh_tools()` at turn/reload boundaries even if config has not
changed. It refetches notified server lists, checks schemas, header annotations,
filters and name collisions again, and publishes a replacement tools snapshot.
A failed refresh preserves the old validated list and remains pending for retry.
Reconnect also rediscovers tools for the next inventory boundary; it never
replays the interrupted effect. A tool removed or changed by reconnect is
rejected before dispatch under its old definition.

`reload` and `refresh_tools` return `McpDiff`, including sorted added, removed
and changed tool names, server lifecycle changes, and bounded diagnostics. The
runner uses those diffs for session notices. `tools()` and `warnings()` return
owned snapshots; discovery and connection state stay inside the host.

A lost call response means an unknown outcome, never a replayed tool POST. With
an earlier SSE stream that supplies event IDs, the client may retrieve the
remaining response using GET and `Last-Event-ID`, honoring SSE `retry` delays.
Each request keeps its own cursor. A missing cursor, unsupported resume, or failed
resume produces an unknown-outcome error. This retrieves response events; it
does not execute the tool again.

Cancellation drops the invoke future, preserving the dispatcher's unknown effect
boundary. Modern HTTP closes its response stream; the older path queues a
best-effort cancellation notification through an owned supervisor. Timeout
returns a completed error saying the outcome is unknown and must not be replayed.
After restart or interruption, unknown effects are never replayed.

Reload cleanup closes a retired generation only after its calls finish. Explicit
host shutdown may interrupt calls; it also covers retired generations. It closes
HTTP requests and sends DELETE for an assigned earlier session, with a bounded deadline. Modern shutdown sends no DELETE. Stdio shutdown
closes stdin, waits up to two seconds, then kills/reaps the owned process tree
(a process group on Unix, a kill-on-close Job Object on Windows), so the server's own
children die with it on both OSes. Dropping the host cancels connections synchronously; explicit shutdown is
needed for HTTP session termination. Stdio stderr stays in a small redacted
in-memory ring, used only for startup diagnostics.

Response bodies and SSE frames are bounded to 8 MiB. Stream fragments and progress
are transient and do not create SQLite rows or WAL writes.

## Goals

### OAuth for HTTP servers

Status: proposed 2026-10-21. Not built; HTTP servers take only `headers_from` today.

- Why: remote servers such as a hosted notes hub (`https://notes.example.com/mcp`) sign in with OAuth. Headless delegates should use them without a hand-copied bearer token.
- Shape: the MCP authorization flow (protected-resource and authorization-server metadata discovery, dynamic client registration, authorization code with PKCE, refresh).
- A person signs in once per server with a runner command that opens the browser and receives the code on a localhost redirect. Runs, including delegates, then use and refresh the stored token without asking.
- Tokens stay outside chat and tool content and outside `config show`, like header credentials. Where they are stored is still open: a user-only file beside the config, or the OS credential store.

## Verification

The in-process HTTP fixture covers modern JSON/SSE discovery and calls, era
fallback, recognized modern rejection, earlier session/header round-trips,
stale-session handling, dropped-call no-replay behavior, response-stream resume,
timeout/cancellation, header annotations, modern and earlier response-stream
list-change notifications, cancellation-safe refresh, and session deletion after
generation drain. Stdio
fixtures cover add/remove/change/forced reload, unchanged-server retention,
invalid reload retention, validated notification refresh, stale and removed
handles, cancelled reload cleanup, and a shared host reload while another session has a call in flight.
Filter fixtures cover discovery and notified refresh, wildcard inclusion and
exclusion, omitted versus explicitly empty inclusion, live cached-list changes,
retained/removed handles, changes with no visible inventory difference, and
connection retention without extra initialize or list requests. Config checks
cover the new keys, layered overrides and empty-list rendering.
Client-environment fixtures cover per-host HTTP authorization headers that differ,
a snapshot miss that does not read the host environment, stdio precedence,
`env_from` resolving only from the snapshot, and a server without `env_from`
that never sees another server's forwarded value.
Config tests cover endpoint and environment-name restrictions. Existing stdio and scripted runner tests retain
approval, effect-recording, interruption, and shutdown coverage.

Acceptance uses Linux with Rust 1.97.1: workspace formatting, strict Clippy,
focused MCP/config tests, and the full workspace suite. No live MCP servers,
OAuth flow, or live model calls are used. The task report records which checks
run and their results; fixture coverage alone does not establish acceptance.
