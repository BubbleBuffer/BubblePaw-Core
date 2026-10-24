# Verification

Status: current as of 2026-10-24.

How BubblePaw is verified now. Dated measurements and investigation logs are not
kept in the repository. Per-provider live results are in
[provider qualification](provider-qualification.md).

## Automated gates

Two CI setups run the same gates:

- **Public repo (BubblePaw-Core).** `.github/workflows/ci.yml` runs on GitHub-hosted
  runners for every pull request and every push to `main`. The `linux` job
  (`ubuntu-latest`) runs fmt, both clippy gates and the tests below, with the runner
  tested once with default features and once with `web,bench`. The `windows` job
  (`windows-latest`, MSVC) runs the core, MCP, process and runner (`web,bench`)
  tests. A `claude-code extension` job runs `bun run typecheck` and `bun test` in
  `extensions/claude-code`. `.github/workflows/release.yml` builds `bubblepaw` for
  Linux and Windows, with default features and with `web`, on every `v*` tag and
  attaches the archives and a `SHA256SUMS` file to a GitHub release.
- **Private development repo.** `ci.yml` runs on every pushed branch on a
  self-hosted runner that keeps `target/` warm; `windows.yml` cross-builds the
  Windows binary from `win/**` branches. Neither runs on pull requests from forks.
  The public workflows are kept under `.github/public/` there and moved into
  place when the tree is exported.

The gates, in order:

- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets --locked -- -D warnings -A deprecated`
- `cargo clippy -p bubblepaw-runner --features web,bench --all-targets --locked -- -D warnings -A deprecated`
- `cargo test -p bubblepaw-core -p bubblepaw-mcp -p bubblepaw-process --locked`
- `cargo test -p bubblepaw-runner --locked` (public CI only)
- `cargo test -p bubblepaw-runner --features web,bench --locked --no-fail-fast`

Each gate runs even when an earlier one fails. CI never calls a paid model.

For a bounded runner sweep, run library/binary tests, each integration target and
doc tests serially with these same features and flags, wrapping each command in
`timeout --kill-after=10s 300s`. Continue after a failed target and record its exit
status; exit status 124 means the command exceeded its deadline. The tracing
fixture also bounds its harness waits so a failure inside a spawned tool task
cannot stall the target.

## Three acceptance levels

Results are reported separately by level:

- **Scripted.** In-process tests with fake or scripted providers and no transport.
- **Local transport.** A loopback HTTP server or child process replays synthetic
  wire streams such as SSE. Web-host tests use real sockets. A recorded live
  response replayed through a fake transport still counts as local transport.
- **Live model.** Real provider calls, in ignored tests that need an explicit
  environment gate. They cost money or quota and are never part of CI.

## What the automated tests cover

- **Core:** SQLite reopen, durable queues, one active run per session, context and
  size guards, cancellation, and rejection of a foreign database. Storage tests
  assert transaction counts and zero writes on reads.
- **Restart safety:** an interrupted external effect is classified as unknown and
  is not repeated on resume (`child_sessions.rs`, `host_attach.rs`).
- **Native tools, runner and web host:** real file edits and processes with output
  bounds; a run persisted and continued by a second process; loopback, Origin, Host
  and JSON boundaries.

Helpers shared by several integration files live once, in `crates/bubblepaw-core/tests/support/` and `crates/bubblepaw-runner/tests/support/`; add new shared helpers there, not in a test file. Tests that wait for state use a condition or event wait (`support/eventually.rs` in the runner) rather than a fixed sleep; fixed sleeps remain only inside polling loops, in fakes that simulate slowness, and in checks that a deadline elapses or nothing happens within a window.

## Live tests

Live tests are `#[ignore]`. Without their gate they print `skipped: <reason>` and
pass. Run them only when the user has approved the spend. Credentials come from
environment variables only, never from files, command lines or logs.

| Test | Gate |
|---|---|
| `provider_live.rs`, codex | `BP_LIVE_CODEX=1` and the existing Codex login |
| `provider_live.rs`, responses | `OPENAI_API_KEY`, or `BP_LIVE_RESPONSES_KEY_ENV` with `BP_LIVE_RESPONSES_BASE_URL` |
| `provider_live.rs`, anthropic_messages | `ANTHROPIC_API_KEY` |
| `provider_live.rs`, opencode_messages, opencode_chat | `OPENCODE_API_KEY` |
| `provider_live.rs`, openrouter_chat | `OPENROUTER_API_KEY` and `BP_LIVE_OPENROUTER_CHAT_MODEL` |
| `provider_live.rs`, deepseek_chat | `DEEPSEEK_API_KEY` and `BP_LIVE_DEEPSEEK_CHAT_MODEL` |
| `provider_live.rs`, claude_code | `BP_LIVE_CLAUDE_CODE=1` and `bun` on `PATH` |
| `provider_images.rs` | `BP_LIVE_IMAGES=1` and `OPENROUTER_API_KEY` |
| `research_live400.rs` | `BP_LIVE_RESEARCH_400=1` and `OPENCODE_API_KEY` |
| `web_live.rs` | `BUBBLEPAW_LIVE_SEARXNG_URL`, `BUBBLEPAW_LIVE_CRAWL4AI_URL`, `CRAWL4AI_API_TOKEN`, as each test needs (no model call) |

Run one provider kind, one test at a time:

```
cargo test -p bubblepaw-runner --test provider_live -- --ignored --test-threads=1
```

Filter by name, for example `opencode_messages::tools`. Override a model with
`BP_LIVE_<KIND>_MODEL`. Each scenario asks for low reasoning effort and stops
before it would exceed its request budget.

## Known limits

- Remote web deployment is not built. The web host binds to loopback and checks
  Host and Origin ([web contract](web-contract.md)).
- Interactive PTY sessions are not built.
- Only still images (PNG, JPEG, GIF and WebP) are read as model input, and only for
  models that accept image input ([README](../README.md)).
- Host tools extend the Rust library; remote tool registration and callbacks are not
  implemented ([README](../README.md)).
- MCP HTTP servers take bearer credentials from the environment. OAuth is not
  implemented ([MCP](mcp.md)).
- BubblePaw never refreshes or writes Codex `auth.json`; the Codex CLI owns refresh
  ([provider qualification](provider-qualification.md)).
- Live coverage is partial. Some provider kinds have never run live, and the
  48-scenario live suite is only partly run.
- An external effect whose outcome is unknown after an interruption is reported as
  unknown and is never repeated automatically. Conversation history cannot undo an
  external action.
