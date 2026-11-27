# Unverified Items

Verified on 2026-11-27 against BubblePaw Core v0.16.1 (Windows, `bubblepaw 0.2.1`) and pi 1.6.1 with `opencode-go/deepseek-v4.1-flash`, using `--help`, the shipped docs and source, and two tiny live probes per agent plus the two-task pilot.

Verified: `bubblepaw --version` and `usage export` (`bubblepaw.usage.v1` rows, exit 0), `bubblepaw.toml` with the OpenCode Go key, stream events (`text_delta`, `paragraph`, `model_request_started`, `tool_started`, `result`), trace fields (`first_text_ms`, `model_finished`, `tool_started` fingerprints); pi `--version` (`1.6.1`), flags `-p --no-session -nc -ne --tools --provider --model --thinking`, provider `opencode-go` / model `deepseek-v4.1-flash`, JSON events and usage fields (`input`, `output`, `cacheRead`, `cacheWrite`, `cost.total`, `isError`). pi's `input` excludes cache; the adapter normalises it (see README).

Still unverified:

1. `bun run --cwd bench/agentic typecheck` needs `typescript` and `@types/bun` installed; neither is in the worktree. `bun test runner.test.ts` passes.
2. `tool_errors` on a failing tool for either agent (no live run produced a tool error); the event fields come from the docs and source only.
3. `cache_miss_unexplained` trace events were not seen live (no such miss occurred); only the `model_finished` fallback rule was exercised.
4. pi's `bash` tool on Windows (needs a shell on PATH) was exercised only through the pilot's tasks, not tested separately.
