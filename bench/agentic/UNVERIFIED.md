# Unverified Items Requiring Shell or Live Run

No shell commands were executed while creating `bench/agentic/`. Please verify these items directly on the host machine:

1. **Offline runner tests & TypeScript typecheck**
   - Run `bun test bench/agentic/runner.test.ts` (verifies all 8 tasks pass under `fake`, all 8 fail under `fake-noop`, `check.ts` is excluded from the workspace, and report formatting works).
   - Run `bun run --cwd bench/agentic typecheck` (`tsc --noEmit -p tsconfig.json`).
   - Run `bun run bench/agentic/run.ts --list`.

2. **Windows subprocess & ESM dynamic import behavior**
   - Confirm `src/check-runner.ts` (`import(pathToFileURL(checkPath).href)`) and `tasks/08-multi-step-migration/check.ts` (`bun run src/cli.ts`) behave cleanly on Windows paths and exit codes.

3. **`pi` CLI flags and OpenCode Go provider/model naming (`src/adapters/pi.ts`)**
   - Confirm installed `pi --version` output format.
   - Confirm exact provider/model flags for DeepSeek V4.1 Flash on OpenCode Go in your local `pi` build (the adapter defaults `opencode/deepseek-v4.1-flash` to `--provider opencode-go --model deepseek-v4.1-flash`, overridable via `--pi-provider` / `--pi-model`).
   - Confirm whether `-p`, `--no-session`, `-nc`, `-ne`, and `--tools read,bash,edit,write,grep,find,ls` match your installed `pi` version without error.

4. **`pi --mode json` event schema (`src/adapters/pi.ts`)**
   - Verify on one live `pi` run that `message_update.assistantMessageEvent.type === "text_delta"`, `message_end.message.usage` (`input`, `output`, `cacheRead`, `cacheWrite`, `cost.total`), `tool_execution_start`, and `tool_execution_end` (`isError`) match the fields emitted by your installed `pi` binary.
   - Verify whether `pi`'s `usage.input` already includes `cacheRead` (as BubblePaw's `input_tokens` does) or reports *uncached* input separately; if `pi` excludes `cacheRead` from `input`, `pi.ts` should normalize `inputTokens = input + cacheRead + cacheWrite` so `cache_hit_ratio` and `estimateCostUsd` match BubblePaw's convention.

5. **BubblePaw live two-task pilot (`src/adapters/bubblepaw.ts` + `bench/agentic/bubblepaw.toml`)**
   - Confirm `bubblepaw --version` output format.
   - Confirm `bench/agentic/bubblepaw.toml` works with your release binary and `OPENCODE_API_KEY` on OpenCode Go (`opencode/deepseek-v4.1-flash`), including `reasoning = { thinking = { type = "enabled" }, reasoning_effort = "low" }`.
   - Confirm `bubblepaw ... usage export` exits 0 and emits `bubblepaw.usage.v1` rows when invoked after `run` with `--data-dir` in `scratchDir`.
   - Confirm `trace.jsonl` records (`first_text_ms`, `cache_miss_unexplained` / `cache_bust`, `tool_started` fingerprint) populate as expected on a live run.
