# Agentic task benchmark

Status: built 2026-11-27 (scripted `fake` harness ready; offline `bun test` and live model pilot pending host execution; see [UNVERIFIED](../bench/agentic/UNVERIFIED.md)).

`bench/agentic/` measures end-to-end coding and workspace agent performance for **BubblePaw** alongside **pi** on identical self-contained tasks and the same model (`opencode/deepseek-v4.1-flash` on OpenCode Go).

## Scope and design

- **No engine changes**: Pure Bun + TypeScript runner, adapters, checks, and Markdown reporter under `bench/agentic/`.
- **Clean workspace isolation**: Only each task's `seed/` directory is copied into a fresh temporary workspace; `check.ts` stays outside and runs in an isolated Bun subprocess after the agent exits. BubblePaw's `--data-dir` and `--trace-file` reside in a separate scratch directory so store/trace artifacts never alter workspace diffs.
- **Adapter boundary**:
  - `bubblepaw`: drives `--json ... run --prompt-file`, reads metadata trace events (`first_text_ms`, cache misses, repeated tool fingerprints), and reads authoritative request usage via `usage export`.
  - `pi`: drives `pi --mode json -p --no-session -nc -ne`, parsing streaming events and token/cost totals (falling back to `crates/bubblepaw-ai/src/catalog/models.json` list prices when an agent omits cost).
  - `fake` / `fake-noop`: deterministic canned-solution and no-op adapters for verifying the runner and all checks without model calls.
- **Variance & truth controls**: Interleaved `(task, agent)` scheduling across repetitions, median latency/token reporting, `cost_per_solved`, `cache_hit_ratio`, `unexplained_cache_misses`, `repeated_tool_calls`, and immutable-file SHA-256 guards.

## Acceptance levels

1. **Scripted harness verification** (`bun test bench/agentic/runner.test.ts`): Verifies the runner, JSONL schema (`bubblepaw.agentic-bench.v1`), workspace isolation, all 8 task checks (`fake` passes all 8, `fake-noop` fails all 8), and Markdown comparison output without model calls.
2. **Live model pilot & baseline**: Run BubblePaw and `pi` against `opencode/deepseek-v4.1-flash`, commit the baseline JSONL under `bench/agentic/baselines/`, and compare future builds with `bun run bench/agentic/report.ts`.

See [bench/agentic/README.md](../bench/agentic/README.md) for commands, task summaries, JSONL fields, and how to add tasks.
