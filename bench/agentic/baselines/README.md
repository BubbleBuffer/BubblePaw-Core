# Recorded Baselines

Committed JSONL baseline runs live here, named by agent, model, and date:

- Example: `pi-deepseek-v4.1-flash-2026-11-27.jsonl`

Record a baseline once (for example, 3 reps per task on `pi` with DeepSeek V4.1 Flash):

```sh
bun run bench/agentic/run.ts \
  --agent pi \
  --model opencode/deepseek-v4.1-flash \
  --reps 3 \
  --out bench/agentic/baselines/pi-deepseek-v4.1-flash-YYYY-MM-DD.jsonl
```

Then compare any BubblePaw run against that baseline without re-running `pi`:

```sh
bun run bench/agentic/run.ts \
  --agent bubblepaw \
  --bubblepaw-bin ./target/release/bubblepaw \
  --model opencode/deepseek-v4.1-flash \
  --reps 3 \
  --out .run/bubblepaw-bench.jsonl \
  --baseline bench/agentic/baselines/pi-deepseek-v4.1-flash-YYYY-MM-DD.jsonl
```

Or compare two existing JSONL files offline:

```sh
bun run bench/agentic/report.ts .run/bubblepaw-bench.jsonl \
  --baseline bench/agentic/baselines/pi-deepseek-v4.1-flash-YYYY-MM-DD.jsonl
```
