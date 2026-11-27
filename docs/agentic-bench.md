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

## Long-context task (opt-in, 2026-11-28)

`09-long-haul` compares how each agent's context compaction holds up when one job produces about 1M tokens of history. It is marked `opt_in` in `task.json`: it runs only when named with `--task`, so the default suite stays eight cheap tasks. A `setup` hook generates the material (3.3 MB of receiving notes from a fixed seed) into the workspace; six rules exist only in the prompt, and `check.ts` scores per stage and per constraint where they were lost. `--context-window <tokens>` limits each agent through its own configuration (BubblePaw: `context.window_tokens` and `context.compact_threshold_tokens` via `--set`; pi: a per-run `PI_CODING_AGENT_DIR` with a `models.json` window override and `settings.json` compaction), and the record gains `context_window`, `compactions` and `peak_request_input_tokens` (additive to `bubblepaw.agentic-bench.v1`). The live pilot (BubblePaw at 100k and 250k, pi at 250k) is not part of the scripted acceptance level. See [bench/agentic/README.md](../bench/agentic/README.md) and [the task's README](../bench/agentic/tasks/09-long-haul/README.md).

## Acceptance levels

1. **Scripted harness verification** (`bun test bench/agentic/runner.test.ts`): Verifies the runner, JSONL schema (`bubblepaw.agentic-bench.v1`), workspace isolation, all 8 default task checks (`fake` passes all 8, `fake-noop` fails all 8) and the opt-in task's setup, check and options, and Markdown comparison output without model calls.
2. **Live model pilot & baseline**: Run BubblePaw and `pi` against `opencode/deepseek-v4.1-flash`, commit the baseline JSONL under `bench/agentic/baselines/`, and compare future builds with `bun run bench/agentic/report.ts`.

See [bench/agentic/README.md](../bench/agentic/README.md) for commands, task summaries, JSONL fields, and how to add tasks.
