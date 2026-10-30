# BubblePaw-Core docs

BubblePaw is a small, library-first Rust agent harness. The core supplies a model and tool loop,
SQLite sessions, steering, follow-ups, cancellation and explicit resume. A thin runner CLI supplies
model adapters and workspace tools; chats, messages and effect records persist in one SQLite store.

Short rules: specs carry a Status line; results pages are dated; edit in place.

## Context management

- [lcm-contract](lcm-contract.md) — Gather/apply facts, trigger mapping and transaction budget; opt-in chapter v2 decisions and sections, exact timeline, prefix preservation and fallback.
- [lcm-episode-v3-comparison](lcm-episode-v3-comparison.md) — Measured 2026-10-28: episode summary prompt v2, the v3 draft and v3.1 (three rounds) (gist + `Actions:` log, `context.lcm.episode_prompt`) on DeepSeek Flash over recorded episodes; verdict keeps v2 the default.
- [lcm-chapter-v2-comparison](lcm-chapter-v2-comparison.md) — Measured 2026-10-29: chapter prompt v1 against v2 (Decisions and Sections blocks, `context.lcm.chapter_prompt`) on DeepSeek Flash over five frozen chapters, two rounds; parse rate, grounding, sections, size, cost and the cause of each fallback.
- [context-compaction](context-compaction.md) — 2026-10-27: durable compaction events, applied-view block tokens, reported/estimated message tokens and SDK reads.
- [memory-files](memory-files.md) — Runner memory files: bounded AGENTS.md and CLAUDE.md context, frozen between compactions and refreshed at apply, with one-time live change notes.
- [skills](skills.md) — SKILL.md catalogs frozen between compactions, ordinary body reads, per-session folder access and durable live diffs.
- [prefix-cache-design](prefix-cache-design.md) — 2026-10-21 P8: every request is logged by tier in the serving store, and an unexplained cache miss warns (`cache_miss_unexplained`) with expected/actual cached tokens and the gap. P7: hosts write per-request context through `ContextWriter` (tail reminders, cold notes); a hook edit to the frozen tier is an undeclared `frozen_change`, a resolver change is declared. P6: same-model side calls (summary, chapter) fork the chat context; fresh side calls declare why. 2026-10-19: prompt-cache accounting (declared busts, unexplained misses per provider/model). 2026-10-18: integrated Chat episode forks, durable runtime notices and native token projection; broader tool declarations and other protocols remain separate, with explicit signed-thinking apply limits.
- [context-classical](context-classical.md) — Built 2026-10-09: classical rolling-summary strategy, selectable beside lcm for replay comparison.
- [decisions](decisions.md) — Dated V0 decision log; live provider apply, default-on completion wake, silent internal workers and child controls (2026-10-14); legacy import copies spill and artifacts (2026-10-21).

## Models and providers

- [thinking-tool-loop](thinking-tool-loop.md) — Fixed 2026-10-14: live signed-thinking 400 from changing budget prefixes; stable history notices, inactive declarations and recorded SSE replay; replay guards, runner inner-loop inventory and the Messages/chat replay boundaries.
- [providers](providers.md) — Built through 2026-10-14: live provider registry reload, provider registry, Responses, Claude Code extension, Anthropic Messages and chat completions with tools, decisions and role effort; every kind qualified to one bar; all HTTP kinds stream server-sent events with batched listener text; reasoning artifacts replay only to their origin provider and model; Claude subscriptions (2026-10-19): per-account extension instances via `env`, API-key guard, `replace` system prompt mode and usage-limit failover; fatal extension errors keep a redacted first line and extension stderr is logged redacted at warn (2026-10-25).
- [provider-qualification](provider-qualification.md) — The qualification bar for provider kinds, with the current status of each kind at the scripted, local-transport and live levels.
- [decision-providers](decision-providers.md) — Approved 2026-10-08; built: decision providers and named model roles that episodes and approvals consume.

## Runtime and API

- [host-api](host-api.md) — One method table, caller read scopes, profiles and stdio/HTTP/CLI/SDK adapters.
- [remote](remote.md) — Device keys, encrypted Noise streams, explicit pairing, JSON peer stores and a multiplexed metadata-only WebSocket relay; runner wiring is deferred.
- [host-context](host-context.md) — Built 2026-10-21: the host preparation hook and `ContextWriter`, where static, derived and live host context goes, and how frozen-tier edits and resolver changes are classified.
- [host-attach](host-attach.md) — 2026-10-21: local run/delegate attachment to the central store's host, per-chat workspaces, request config and host lifetime; verification status and Windows smoke commands.
- [sdk](sdk.md) — Updated 2026-10-30: `HostConfig::load` and `Client::open_configured` for the CLI host; context events, view blocks, message-token reads, and host answers and pending reads for child questions; the Rust facade, `HostBuilder`, Claude Code helper, todo module and stability policy.
- [storage-model](storage-model.md) — Updated 2026-10-27: schema 12 compaction facts and message usage links without extra ordinary transactions; central storage, legacy import, lean chats, archives and disposable serving.
- [session-api](session-api.md) — Built; current 2026-10-29: stdio provider and main-role controls, paged session lists and child cancellation; session snapshots, durable events, Context reads and direct-parent child events.
- [usage tracking](usage-tracking.md) - Durable per-request tokens and SDK-reported model IDs, one dated price table, grouped reports and the fixed `bubblepaw.usage.v1` JSONL export.
- [quota](quota.md) — Native subscription/rate-limit/credit readings, change-only history, rejection-specific waits and restart recovery; piggybacked balances with explicit standalone recording times; OpenCode Go meters through a BubblePaw-owned console login (`bubblepaw login opencode`).
- [rewind](rewind.md) — Built 2026-10-15; edit/write updated 2026-10-20: file checkpoints, content-addressed originals, conflict-safe session rewind, and rewind limited to the latest turn (a typed user row or ChildTask instruction; host notices never count), with checkpoints pruned to the current and previous turn.
- [web-contract](web-contract.md) — Built; current 2026-10-27: `web` startup line (`--port 0`), local boundary checks, every JSON endpoint, SessionView shape, SSE and error statuses, including user answers to child questions.
- [web-workbench](web-workbench.md) — Built through 2026-10-14: automatic completion turns, older-history pagination and provider setup with live apply and explicit disk reload.
- [product](product.md) — Workbench product brief: users, purpose, constraints, brand commitments and principles.
- [design](design.md) — Workbench visual specification: tokens, colors, typography, layout and components.
- [child-worktrees](child-worktrees.md) — Built 2026-10-15: opt-in child git workspaces, safe archive/delete cleanup and explicit continuation.
- [child-sessions](child-sessions.md) — Built through 2026-10-27: durable run/question projections on the direct parent's event stream; user answers with atomic resolution, attribution and parent notice; recursive controls, acknowledgement and retention; hidden workers stay silent toward their callers.
- [tracing](tracing.md) — Experimental, optional: disposable JSONL behavior tracing, off by default, with metadata or contents modes; prompt-cache events.
- [config](config.md) — Built through 2026-10-21: central default data dir, layered TOML, live provider apply and explicit reload, default-on completion wake/debounce, worker retention, chat reasoning replay and stale-child reminders.
- [local-worker](local-worker.md) — Local Windows worker launch (own data dir; shared dirs attach), OpenCode Go profile, task-file input and full Windows/live LCM qualification (2026-10-18).

## Approvals and tools

- [file-tools](file-tools.md) — Five workspace tools: read/list/search, literal edit and whole-file write; atomic writes, checkpoints and concurrent-write protection; 2026-10-20 read defaults (1000 lines) and argument/path diagnostics.
- [approvals](approvals.md) — Built 2026-10-08: tool-call approval gate with ask, allow, deny and auto modes.
- [execution modes](codemode.md) — Native default (2026-10-22), persisted chat modes and opt-in JS wrapper; shared dispatch, core/deferred tools, tool_search/call_tool, frozen declarations, server anchors and reload notices.
- [mcp](mcp.md) — Built 2026-10-14: MCP client tools over stdio and Streamable HTTP (2026-07-28 with session-era fallback), env-only headers, approval by default, unknown outcomes never replayed. Goal: OAuth for HTTP servers (proposed 2026-10-21).
- [live-config](live-config.md) — Updated 2026-10-30: live model, read-only and compaction controls with per-key restart notices; turn-start config refresh, shared reload commands, live settings, MCP generation draining and tool-list updates, and one-time durable diffs; hooks and plugins deferred.
- [exec](exec.md) — Built through 2026-10-20: larger previews, five-minute sync default, ten-minute process waits, aliases, completion wake and process ownership.
- [web](web.md) — Research update 2026-10-13: visible research children, direct parent search/fetch, root-chat source IDs, numbered passages with BM25 and disposable shared SQLite cache.

## Verification and experiments

- [chapter-bench](chapter-bench.md) — How to run the chapter-selection benchmark: arms, cost cap, frozen episode stages and the report. Results of 2026-10-23 are kept in the project notes.
- [delegation-bridge](delegation-bridge.md) — Built 2026-10-19 (v0): `bubblepaw delegate` runs an assignment with harness-run checks and rework, false-green detection, explicit class or model routing with in-class exploration, and a `delegations.db` ledger with metrics; `bubblepaw delegations report` groups outcomes by category, class and model.
- [verification](verification.md) — How the project is verified now: CI gates (private CI also runs the claude-code extension's bun checks, 2026-10-25), the three acceptance levels, opt-in live tests and known limits.

## Ideas

- [ideas](ideas.md) — Idea status, including native web research promoted to host support.
