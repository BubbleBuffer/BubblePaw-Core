# Attach to a running host

Status: implemented; MCP environment isolation and Windows stack repair added, 2026-10-21.
Option 2 is authorized: workspace ownership belongs to each root chat. Windows
build and smoke testing remain the user's checks; verification is recorded below.

## Current state

Without `--data-dir`, every workspace now uses the one central store
`<local data home>/BubblePaw-Core/store`, so the first BubblePaw process on the
machine hosts all later `run` and `delegate` work regardless of workspace. On its
first start that host imports the old per-workspace stores (see
[storage model](storage-model.md#central-store-and-legacy-import-2026-10-21)).
The host publishes `host.json` before that import; a client that attaches
meanwhile prints `waiting for the host to finish importing legacy stores` once
and waits for the import instead of failing after the five-second attach wait.
If the importing process is a one-shot command, it tells waiting clients to
elect again; their requests were never read.
The import copies each old store's chats, spill output and artifacts, so after
it an old store directory can be deleted, except one whose child worktrees are
still in use.

The existing core process lock elects one host per data directory. Its semantics
are unchanged. A later `run` or `delegate` connects to that host and executes an
ordinary session there. It does not open a second store. Commands that need the
store but do not support attachment retain their ownership error, now identifying
the published host PID. Existing commands that do not acquire the store, such as
`delegations report`, keep their existing behavior. `--no-session` uses its own
temporary store.

The host publishes `<data-dir>/host.json` only after opening its control endpoint:

```json
{"pid":1234,"endpoint":"/path/to/data/host.sock","protocol_version":2}
```

On Unix, the socket is inside the data directory. The directory is owned by the
current user and restricted to `0700`; the socket is `0600`. Both sides check peer
UIDs. Windows uses a randomly named local pipe with a protected DACL granting
access only to the current user's SID, and rejects remote clients. The Windows
data directory is not ACL-checked and may be writable by other local users (for
example on a non-system NTFS drive), so a client does not trust the pipe name in
`host.json` alone: after opening it, the client looks up the serving process
(`GetNamedPipeServerProcessId`) and refuses to attach, with an error naming that
process, unless it runs as the current user's SID. The transport uses existing
Tokio and windows-sys dependencies, with their required features (the
`Win32_System_Pipes` feature, which Tokio already enables on the same
windows-sys version, is now declared explicitly); no new dependency was added.

A client waits up to five seconds for an owner that is still publishing. Before
submission it may retry the existing lock if that owner has already exited. Once
submission starts, it never reconnects or repeats the request: the host may have
already accepted input or dispatched an external effect. Protocol versions must
match. A crashed owner leaves an unlocked data directory; its successor replaces
the descriptor and stale Unix socket after acquiring the unchanged core lock.

## Request isolation

The invoking client resolves its prompt or assignment, CLI flags and config
layers before attachment. Relative configuration paths are anchored to that
client's invocation directory. Each request constructs its own providers, tools,
approval policy, context services and child registry against the shared Store.
Config reload retains the client's layer paths and config overrides. The full
local environment is excluded from serialization. Attachment forwards only
credential variables named by the resolved configuration: provider `key_env`,
web credential references (including `CRAWL4AI_API_TOKEN` when Crawl4AI is
configured), and MCP `env_from`/`headers_from`. Missing values stay
absent. Unrelated client variables do not travel over the endpoint. Implicit
client subprocess environment customizations are not forwarded; use explicit
extension `env` or provider `auth_file` settings for those inputs.
The client's default Codex auth and model-cache paths are resolved before
submission, preserving those paths without forwarding HOME or CODEX_HOME.

Each request owns its MCP host, processes and connections. Only MCP-referenced
values enter its `McpClientEnv`; provider credentials are not added to this MCP
snapshot. `env_from` and `headers_from` resolve from that snapshot exclusively,
including after reload and reconnect. A missing variable produces the same
diagnostic as standalone startup, even if the owner has it. Existing startup
behavior still omits an unavailable server's tools and reports a warning.

Attached MCP subprocesses inherit the host environment, followed by explicit
`env` and `env_from`. A forwarded value reaches only the server whose own
`env_from` (or, for HTTP, `headers_from`) names it; server A's forwarded token
never reaches server B. There was
no separate MCP forwarding allowlist: `header_key_envs` validates header names;
it does not select inherited variables. Different requests always have separate
server instances, including when the server configuration matches but credential
values differ. Forwarded MCP values are redacted from MCP diagnostics and tool
results, and are never stored in the catalog, chat files or traces.

Other attached subprocesses (extensions, `exec`, web helpers) inherit the host
environment with the forwarded non-MCP references (provider and web credentials)
overlaid, then their configured overrides. A value referenced only by MCP
configuration is not added to their environment; a name referenced by both MCP
and a provider is, as a standalone subprocess would see it. No request changes
the process-wide environment.

Children of attached sessions therefore inherit the base environment of the
process that started the host, not the attaching client's. Both run as the same
user; this is intended.

A new root chat atomically stores `root_workspace:<root-id>` in its authoritative
chat metadata. Descendants inherit that workspace unless the existing child
worktree feature supplies their execution directory. The persisted binding is
the source of truth for workspace resolution and protection. An existing session
must be invoked using its persisted workspace; the host's startup workspace is
never added to another session's protected roots. Catalog rebuild accepts roots
with different bindings, and rejects malformed bindings.

Old chat-level `workspace` metadata and the old catalog-level binding remain
readable fallbacks. Opening or rebuilding does not rewrite old chat files to add
bindings. New roots also persist their storage retention and cold-storage policy
in `root_storage:<root-id>`; housekeeping applies each root's policy in one
serving transaction. Roots without that metadata retain legacy store defaults.

Request event listeners track current session ownership. An attached run cannot
be auto-denied or cleaned up by another request's event listener. Providers and
request resources drain independently; shared `Harness::shutdown` runs once at
host exit.

## Control protocol, version 2

The endpoint carries newline-delimited JSON-RPC 2.0, reusing stdio's envelopes and
`event` records. It is private local process control, not a network service.
Records have the same 1 MiB bound as stdio requests.
Version 2 replaces version 1's full environment snapshot with explicit references.
Mixed-version processes reject attachment; restart an older host before attaching.

1. The client sends one `attach.submit` request with an ID. Its parameters are the
   resolved request: command/flags, effective configuration and layer provenance,
   workspace/data paths, prompt, and resolved assignment/route for delegation.
   Only `run` and `delegate` are accepted. The receiving host validates the command,
   data directory, workspace and config before creating the session.
2. The host sends ordinary `event` notifications with serialized `HarnessEvent`
   parameters. `attach.output` notifications carry `{stream:"stdout"|"stderr",
   text:...}` for CLI text and final CLI records. The client uses the same event
   presenter as a standalone command. Event sequences are host-wide; UUIDs and
   timing fields naturally differ between runs.
3. The response to `attach.submit` carries `{exit_code,status}`. Status is
   `completed`, `failed` or `cancelled`; stdout/stderr already contain the ordinary
   command result and diagnostics. Validation failures use JSON-RPC error -32602.
4. A client Ctrl-C sends the existing `run.cancel` method with empty parameters.
   The client arms its interrupt listener before it writes `attach.submit`, so an
   early or between-poll Ctrl-C is buffered instead of killing the client or
   being lost. On this endpoint it applies only to the connection's submitted work. EOF or a
   broken output stream cancels the same request. It never cancels an unrelated
   session. A bounded output queue and write timeout prevent a stalled reader
   from retaining the host indefinitely.

The filtered configuration snapshot can include referenced client credentials.
It travels only over the user-restricted endpoint and is not written to host.json
or the delegation ledger. Treat raw control frames as sensitive.

## Delegation and lifetime

Assignment execution, acceptance checks, reworks and every ledger write occur in
the host. `delegations.db` contains one transaction per attempt, including failed
and cancelled attempts. Cancelling during a check stops its process tree and
prevents another check or rework from starting.

There is no daemon. The host exits after its own command and all admitted clients
finish, even when its own command finishes first. Normal exit removes `host.json`
and the Unix socket before releasing the store lease. Host death makes attached
runs fail as interrupted, with no automatic retry or replay of unknown effects.
A forced kill cannot run descriptor cleanup; the next elected host replaces it.

## Verification

### Windows stack repair

The user's Windows debug build at `07c70a3` overflowed the 1 MiB entry-thread
stack before publishing `host.json`. The CLI, host request, and command futures
now use heap-pinned boundaries. Windows also runs the Tokio runtime on an
explicit 8 MiB thread; this bounds the entry-thread requirement even when MSVC
debug poll frames remain large. The process waits for that thread and preserves
its exit result. There is no detached runtime thread.

The Linux process regression runs both an owner and attached client with
`RLIMIT_STACK=1 MiB`, waits for successful completion, and checks descriptor
removal. It passes; it also passed before the repair, so it does not reproduce
the Windows compiler's stack use. Native Windows verification remains pending.

### Initial host attachment

Verified on Linux at `07c70a3`, 2026-10-21:

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- `cargo test -p bubblepaw-core`: passed, including workspace, legacy-store,
  rebuild, protected-root and per-root storage-policy coverage.
- `cargo test -p bubblepaw-runner`: passed, including all ten real-process
  host-attach tests and delegation cancellation/check/ledger tests.
- Web-provider unit tests (four tests) and `cargo check -p bubblepaw-runner
  --features web`: passed.
- Web-feature integration tests: 22 provider tests and 11 transport tests passed,
  including credential non-disclosure and rejection of foreign-workspace roots.
  Strict Clippy for the web-feature library and binary also passed.

An earlier full runner pass hit a ten-second initial stdio RPC timeout once.
The isolated test, complete seven-test runner target and subsequent full suite
passed without a timing or production-code change. Native Windows checks and
gated live-provider tests were not run. The private transcript fixture was
unavailable (`BUBBLEPAW_FIXTURES_DIR` was unset).

### MCP environment follow-up

The new real-process tests in `tests/host_attach_mcp.rs` pass on Linux: two
attached requests send their own HTTP credentials, run independent stdio servers
with their own `env_from` values and host inheritance, omit unrelated client variables, and
leave no credential values in stores, contents traces or CLI output. A missing
client variable produces the standalone diagnostic despite an owner value.
All eleven host-attachment process tests pass, including the 1 MiB stack test.
MCP library fixtures additionally cover redaction, missing-variable errors,
platform name matching, forced reload and crash reconnect.

Verified on Linux, 2026-10-21 (clean sync): `cargo fmt --all -- --check`
passed; `cargo clippy --workspace --all-targets -- -D warnings` passed;
`cargo test -p bubblepaw-mcp -p bubblepaw-runner --no-fail-fast` passed
(1310 passed, 0 failed, 60 ignored across 127 targets). Native Windows checks
and live-provider tests were not run.

Linux checks must run from Git Bash in the `host-attach` worktree:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p bubblepaw-core
cargo test -p bubblepaw-mcp -p bubblepaw-runner
```

The scripted process tests are in `crates/bubblepaw-runner/tests/host_attach.rs`.
They exercise concurrent runs, per-client model/approval/workspace/environment,
stdout/stderr parity, concurrent delegations and reworks, catalog rebuild, host
lifetime, cancellation, disconnection, host death, endpoint permissions and PID
diagnostics. Core tests cover mixed workspaces, unchanged legacy chat files,
rebuild, child inheritance, per-session protected roots and storage policies.
These tests do not establish live-model acceptance.

Windows build and smoke commands (run by the user, in this worktree):

```powershell
cargo build -p bubblepaw-runner --bins
cargo test -p bubblepaw-runner --lib host_transport
pwsh -NoProfile -File crates/bubblepaw-runner/tests/host_attach_windows.ps1
```

The smoke script resolves Cargo's configured target directory automatically.
Use `-BinaryDirectory <path>` to select a different binary directory.
It starts a scripted host and client in separate workspaces,
checks that the pipe stays owned by the first PID while attached work runs, then
checks normal metadata removal. Its `finally` block kills only its own remaining
process trees and removes only its generated temporary directory. Windows pipe
ACL enforcement and native compilation are not claimed until those checks run.
