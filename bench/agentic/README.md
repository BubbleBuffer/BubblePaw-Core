# BubblePaw Agentic Benchmark

Self-contained benchmark suite comparing **BubblePaw** against **pi** (or any adapter behind the same interface) on the same tasks and model (`opencode/deepseek-v4.1-flash`).

## Quickstart

List tasks:

```sh
bun run bench/agentic/run.ts --list
```

Verify the runner and all 8 default task checks offline (plus the opt-in `09-long-haul`) (uses `fake` and `fake-noop` adapters, zero model calls):

```sh
bun test bench/agentic/runner.test.ts
```

Typecheck the runner, adapters, checks, and tests:

```sh
bun run --cwd bench/agentic typecheck
```

Run a two-task live pilot on BubblePaw (requires `OPENCODE_API_KEY` and a built `bubblepaw` binary):

```sh
bun run bench/agentic/run.ts \
  --agent bubblepaw \
  --bubblepaw-bin ./target/release/bubblepaw \
  --task 01-one-line-bug --task 04-read-and-answer \
  --out .run/pilot-bubblepaw.jsonl
```

Run BubblePaw and `pi` interleaved across all 8 tasks (3 reps each) and save to `.run/`:

```sh
bun run bench/agentic/run.ts \
  --agent bubblepaw --agent pi \
  --bubblepaw-bin ./target/release/bubblepaw \
  --model opencode/deepseek-v4.1-flash \
  --reps 3 \
  --out .run/compare-all.jsonl
```

Compare any results JSONL against a committed baseline:

```sh
bun run bench/agentic/report.ts .run/bubblepaw-run.jsonl \
  --baseline bench/agentic/baselines/pi-deepseek-v4.1-flash-YYYY-MM-DD.jsonl
```

## Task Suite (8 Tasks, plus one opt-in)

| ID | Category | Expected steps | What it tests |
| --- | --- | --- | --- |
| `01-one-line-bug` | `find-fix` | 2-4 | Touching & nested interval merges without mutating inputs |
| `02-implement-lru` | `write-against-tests` | 4-7 | TTL-aware `LruCache<K, V>` against `SPEC.md` + hidden edge cases |
| `03-rename-across-modules` | `multi-file-refactor` | 6-10 | Rename `AccountId` / `amountCents` across 7 files while keeping `"amount_cents"` wire key |
| `04-read-and-answer` | `read-answer` | 5-8 | Read ADRs/configs/code to report effective prod policy in `ANSWER.json` without edits |
| `05-shell-log-forensics` | `shell-use` | 6-10 | Deduplicate multi-file JSONL logs, skip malformed lines, compute exact nearest-rank p95 |
| `06-constraint-fix` | `constraint-respect` | 5-8 | Fix banker's rounding & caps while leaving frozen config/tests untouched and no extra files |
| `07-edit-precision-trap` | `harness-trap` | 8-14 | Fix AST compiler bugs amid repeated `switch` blocks and noisy test stdout |
| `08-multi-step-migration` | `long-horizon` | 14-22 | Multi-file queue upgrade: priority lanes, DAG deps/`CycleError`, retries, dead/blocked cascade, CLI |
| `09-long-haul` | `long-context` | 300-800 | **Opt-in.** Twenty generated stages of receiving notes (about 1M tokens of history); six rules given only in the prompt must hold in every stage file. Scores recall across compaction per stage and per constraint |

### Long-context task `09-long-haul` (opt-in)

It runs only when named with `--task 09-long-haul` (a run without `--task` keeps the 8 tasks; `--list` marks it).
`task.json` sets `"opt_in": true`, a `"setup"` hook (the runner runs `bun <setup> <workspace>` after copying the seed,
and the generated material does not count in `files_changed`), `timeout_ms` 5400000 and `max_rounds` 1500
(`--timeout-ms` and `--max-rounds` override them). The notes are
generated from a fixed seed; the rules are only in the prompt. Details, the token estimate and how the check scores:
[tasks/09-long-haul/README.md](tasks/09-long-haul/README.md).

Planned comparison runs (same model; needs `OPENCODE_API_KEY`; one at a time, each about an hour):

```sh
bun run bench/agentic/run.ts --agent bubblepaw --bubblepaw-bin ./target/release/bubblepaw --task 09-long-haul --context-window 100000 --out .run/long-bp-100k.jsonl
bun run bench/agentic/run.ts --agent bubblepaw --bubblepaw-bin ./target/release/bubblepaw --task 09-long-haul --context-window 250000 --out .run/long-bp-250k.jsonl
bun run bench/agentic/run.ts --agent pi --pi-node <node.exe> --pi-bin <.../pi-coding-agent/dist/bundle/cli.js> --task 09-long-haul --context-window 250000 --out .run/long-pi-250k.jsonl
```

## Runner CLI Options

- `--agent <name>`: `bubblepaw`, `pi`, `fake`, or `fake-noop` (repeatable or comma-separated)
- `--task <id>`: filter to specific task IDs (repeatable or comma-separated)
- `--reps <n>`: repetitions per `(task, agent)` (default `1`)
- `--model <provider/model>`: default `opencode/deepseek-v4.1-flash`
- `--bin`, `--bubblepaw-bin`, `--pi-bin`: override agent executable paths
- `--bubblepaw-config <path>`: TOML config for BubblePaw (default `bench/agentic/bubblepaw.toml`)
- `--pi-provider <name>`, `--pi-model <id>`, `--pi-tools <list>`, `--pi-extra-args <csv>`: explicit `pi` overrides
- `--timeout-ms <ms>`: per-run timeout; when given it overrides the task's own `timeout_ms` (otherwise the task's, else `180000`)
- `--max-rounds <n>`: override `agent.max_rounds` for BubblePaw (a task's own `max_rounds` applies when this is not given)
- `--context-window <tokens>`: limit every agent to this context window, through its own configuration (below); the run's compaction follows from it. Minimum 20000
- `--out <path>`: JSONL output path (streams one record per completed run)
- `--baseline <path>`: optional baseline JSONL for comparison report
- `--no-interleave`: disable alternating agent execution per task/rep
- `--seed <n>`: deterministic shuffle seed within interleaved reps
- `--keep-workspaces <dir>`: preserve final workspaces and raw stdout/stderr/trace artifacts

## JSONL Output Schema (`bubblepaw.agentic-bench.v1`)

Each line in the output JSONL contains:
- Identity & outcome: `schema`, `timestamp`, `task`, `task_category`, `agent`, `agent_version`, `model`, `rep`, `passed`, `check_details`
- Timing & usage: `wall_ms`, `first_text_ms`, `requests`, `input_tokens`, `output_tokens`, `cached_tokens`, `cache_creation_tokens`, `reasoning_tokens`, `cost_usd`, `cost_source`
- Context (additive, still v1; null when not applicable or not observable): `context_window` (the `--context-window` of the run), `compactions` (count, from each agent's own events), `peak_request_input_tokens` (largest single request prompt, cache included). `report.ts` prints a "Context Window And Compactions" table when any record has them
- Execution & harness diagnostics: `tool_calls`, `tool_errors`, `exit_code`, `error`, `timed_out`, `files_changed`, `cache_hit_ratio`, `unexplained_cache_misses`, `repeated_tool_calls`, `answer_chars`

## Comparing the two agents

- **Token convention.** `input_tokens` is the whole prompt including cache reads and writes, for both agents. BubblePaw reports it that way (`usage.input_tokens` = uncached + cache reads + writes). pi's own `usage.input` excludes cache (pi-ai: `input = prompt_tokens - cacheRead - cacheWrite`), so the pi adapter adds `cacheRead + cacheWrite` back. `cached_tokens` is the cache-read part of `input_tokens` in both.
- **Cost.** BubblePaw: `bubblepaw usage export` (its price table). pi: its own catalog price (`cost.totalFromProvider: false`), recorded as `cost_source: "catalog"`. Neither is the provider's invoice.
- **Equal settings.** Model `deepseek-v4.1-flash` on OpenCode Go for both (`opencode/...` in BubblePaw, provider `opencode-go` in pi); reasoning low for both (`reasoning_effort = "low"` in `bubblepaw.toml`, `pi --thinking low`). Tools differ by design: BubblePaw's native set vs pi's `read,bash,edit,write,grep,find,ls`. pi runs with no context files, extensions, skills or prompt templates, and `--offline`.
- **pi on an older Node.** pi needs Node 22.19+. Run it on a portable Node with `--pi-node <node.exe> --pi-bin <.../pi-coding-agent/dist/bundle/cli.js>` (or `PI_NODE`/`PI_BIN`).
- **Not reported.** pi has no `unexplained_cache_misses` (null). `first_text_ms` is the first assistant text delta of the run, which can be a narration before tool calls. `requests` is model requests (BubblePaw: usage rows, which also count side calls such as summaries; pi: assistant messages).
- **`unexplained_cache_misses`** (BubblePaw) counts `cache_miss_unexplained` trace events; the fallback uses BubblePaw's own rule (at least 1024 expected cached tokens and a shortfall over max(128, 10%)), not provider block rounding.

## Context window per agent

`--context-window W` is applied only through each agent's configuration; neither agent is patched.

- **BubblePaw:** `--set context.window_tokens=W --set context.compact_threshold_tokens=W` on the command line (on top of `bench/agentic/bubblepaw.toml`). BubblePaw compacts at the smaller of the cap and `W` minus `context.lcm.output_reserve_tokens` (16000), so the threshold is about `W` - 16k. `compactions` counts `context_compaction` events of kind `applied` in the `--json` event stream. It is null when summary requests exist but no event was seen. `peak_request_input_tokens` is the largest `input_tokens` of a `usage export` row (summary requests included).
- **pi:** a per-run agent directory `<scratch>/pi-agent`, passed as `PI_CODING_AGENT_DIR` (the user's own `~/.pi/agent` is untouched; the API key comes from `OPENCODE_API_KEY`). Its `models.json` overrides the built-in model: `providers.opencode-go.modelOverrides["deepseek-v4.1-flash"].contextWindow = W`; its `settings.json` keeps `compaction.enabled` on. pi compacts when context exceeds `W` - `compaction.reserveTokens` (16384 by default). `compactions` counts `compaction_end` events with a result; the summary request's usage (when pi reports it) is added to the totals and to `requests`. `peak_request_input_tokens` is the largest assistant message prompt (input + cache read + cache write).
- Without the option nothing changes: BubblePaw keeps `bubblepaw.toml`'s window and pi its own agent directory.

## Adding a Task

Create `bench/agentic/tasks/<id>/`:
1. `task.json`: `{ "id", "title", "category", "expected_steps", "timeout_ms", "description" }`, optionally `max_rounds`, `opt_in` (only runs when named with `--task`) and `setup` (a script run as `bun <setup> <workspace>` after the seed is copied, for generated material)
2. `prompt.md`: exact prompt sent to the agent
3. `seed/`: initial workspace copied into a clean temp directory before each run (must fail `check.ts` initially)
4. `check.ts`: exports `async function check(ctx: CheckContext): Promise<CheckResult>` (never copied into the agent's workspace)
5. Register a matching deterministic solver in `src/adapters/fake-solutions.ts` so `bun test bench/agentic/runner.test.ts` continues to validate both `fake` (pass) and `fake-noop` (fail).

## Recording a Baseline

See [baselines/README.md](baselines/README.md). Commit only real model runs with the exact `agent_version`, `model`, and date recorded in the JSONL records.

## Making the Benchmark Tell the Truth

1. **Interleave reps instead of running agents back-to-back**: OpenCode Go latency, provider routing, and prompt-cache warmth vary by time of day. Running `bubblepaw` and `pi` alternately per task and rep (enabled by default) keeps API conditions symmetric across both arms.
2. **Track `cost_per_solved` and `cache_hit_ratio`, not just pass rate**: A harness that solves 7/8 tasks by burning 4x uncached input tokens because of unstable prefix construction (`unexplained_cache_misses > 0`) looks fine on pass rate alone and terrible in production.
3. **Measure `repeated_tool_calls` and `first_text_ms`**: Repeated identical tool fingerprints reveal tool-loop spinning or flaky edit matching; `first_text_ms` catches harnesses that stall before streaming.
4. **Prevent check-leakage and fake-greens**: Only `seed/` is copied into the temp workspace; `check.ts` runs afterwards in an isolated Bun subprocess, verifies immutable files by SHA-256 (`04`, `05`, `06`, `07`), checks for stray temp files (`06`), and asserts hidden edge cases not present in `seed/tests/`.
5. **Isolate BubblePaw state per run**: `--data-dir` and `--trace-file` live in a scratch directory outside `workspace/`, so SQLite files (`catalog.db`, `serving.db`, `chats/`) never pollute `files_changed` or workspace searches.

## Next Ideas

- **Pass^k and flakiness reporting**: Run `--reps 3` or `--reps 5` and report both `pass@1` (mean pass rate) and `pass^k` (solved on every repetition) to distinguish reliable harnesses from lucky ones.
- **Subprocess cancellation / timeout recovery task (Task 10)**: A task where an initial test command hangs unless bounded with a timeout, testing whether the harness's exec timeout/diagnostics let the agent recover cleanly instead of burning its whole budget.
- **Diff-only / minimal-footprint scoring**: Record total changed lines alongside `files_changed` to catch agents that rewrite whole files or reformat unrelated modules.
- **Frozen replay / cost-cap guard**: Add `--max-total-cost-usd` in `runner.ts` that stops scheduling further runs once cumulative recorded spend crosses a budget.
