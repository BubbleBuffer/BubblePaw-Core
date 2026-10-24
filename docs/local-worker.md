# Local BubblePaw workers

**Status:** the repaired [LCM contract](lcm-contract.md) is installed 2026-10-19.
Local contract checks and a fresh synthetic root automatic-pressure/post-apply
run establish its exercised behavior. Earlier full-suite/child/coding acceptance
below is dated evidence, not a fresh qualification of this exact executable.

The existing runner can act as a worker launched by another agent's process tool.
It receives a task and workspace, starts with its own history, and returns JSONL
events followed by a `type: "result"` record containing status, answer, usage and
session ID. It does not inherit the caller's conversation. A caller should check
the final run status and exit code before accepting the answer.

## Windows setup

From this repository:

```powershell
cargo build --locked --bin bubblepaw -j 2
& .\target\debug\bubblepaw.exe --workspace C:\work\project `
  --config examples\worker-opencode-go.toml --json `
  run --prompt-file C:\work\task.txt
```

Use a release or installed binary in place of `target/debug/bubblepaw.exe`,
`--config` for another profile, or `--model provider/model` to change the
main model. OpenCode credentials come from the inherited `OPENCODE_API_KEY`.
The example tries `opencode-free/deepseek-v4.1-flash-free`, then
`opencode/deepseek-v4.1-flash` on Go. Summary/chapter roles follow the main route;
decisions use OpenCode `jev-1.13-free`, then `jev-1.13`. A successful main fallback
sticks for that source session, avoiding repeated unavailable-free calls.
Its API aliases can change upstream; successful calls do not attest a particular
DeepSeek checkpoint.

Search is enabled with DeepSeek first and DuckDuckGo fallback. This local profile
reads `DEEPSEEK_MODEL_API_KEY`; the generic setting defaults to
`DEEPSEEK_API_KEY`. Set `web.deepseek_key_env` to select another variable.

Live qualification on 18 October: both Jev routes returned successful decisions.
The free DeepSeek Flash route returned HTTP 403 `FreeTierError` for this client, so Go is
the usable fallback. DeepSeek search initially returned HTTP 402
`insufficient_balance_error`. After funding, the actual BubblePaw search client
returned three Rust documentation results with cache and fallback disabled.
Fallback, accounting and search transport are also covered by local fixtures.

Historical Windows qualification on 18 October: the binary was rebuilt after
these changes. The corrected full
workspace/all-target/all-feature suite passed 1,377 tests, with zero failures
and 61 ignored; formatting, strict Clippy and the final all-feature CLI build
passed. The same executable passed ten live checks for background root/child
LCM gathering, evidence recall after automatic/manual apply, restart persistence,
durable steering, native child tools and actual DeepSeek search. It was archived
before installation as the helper's default binary. The final build also fixes
yielding polls being cut by CodeMode's ordinary non-wait deadline; a separate
real 30-second script-only poll retained state and completed the same process.
The frozen benchmark predates this last poll repair. The small synthetic check
does not establish general recall quality, implementation quality or quota savings.
See [verification](verification.md) for the checks that run.

The example permits unattended tools for the caller's authorized workspace task.
`--read-only` removes commands and writes. It retains ordinary repository memory
and skills; it does not erase project instructions to reduce the prompt.
Use `--set agent.max_rounds=48 --set context.strategy=classical` for layered
overrides. Root prompt replacement, reasoning fields, approvals and context
settings are described in [configuration](config.md).

Pass `--data-dir <path>` for an archive separate from the central
`BubblePaw-Core/store`; such an archive is not imported into it. Resume with the
same workspace and `run --session <id>` plus a new task file. Each archive has one
live owner; a simultaneous worker using the same data directory attaches its `run`
to that owner as [host](host-attach.md) instead of failing, so workers no longer
need different data directories. `--trace-file <path> --trace-mode metadata`
records a per-run behavior trace. The runner is
a process entrypoint; native child workers inside one host remain a separate
existing interface. It does not install a service or change global provider routing.

PowerShell example for a calling agent:

```powershell
& <repo>\target\debug\bubblepaw.exe `
  --workspace C:\work\project --data-dir C:\work\worker-state\task-a `
  --config <repo>\examples\worker-opencode-go.toml --json `
  run --prompt-file C:\work\task.txt > C:\work\result.jsonl
```

## Live acceptance, 17 October 2026

On this PC, a locally built Windows runner used
`opencode/deepseek-v4.1-flash` through the Go chat-completions route. In a new
scratch workspace it read an intentionally unfinished Python interval-merging
function and existing tests, edited the implementation, and ran
`python -m unittest -v` through PowerShell. Five tests failed before the run;
all six passed afterward, independently rerun by the caller. The tests were not
changed. The run completed in 8.3 seconds with 8,912 input tokens, including
4,864 cache-read tokens, and 954 output tokens (249 reasoning tokens reported).
This first proof used positional task input, with memory/skills disabled only
for the synthetic scratch fixture. Subsequent source trials used the helper's
task-file surface, custom root prompt, real repository memory/skills and
read-only tools.

This proves a small tool-using coding task, not large-repository implementation,
research quality, long-session compaction or a subscription savings ratio.
Original transcripts and usage remain in the caller's isolated proof archive.

A narrow source question about disabled thinking and effort emission was then
run twice in fresh sessions on the real repository. The second run changed only
the worker prompt to prefer exact searches, relevant ranges and decisive evidence:

| Trial | JS cells | Total input | Cache-read input | Uncached input | Output | Reasoning reported | Seconds |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Initial worker prompt | 7 | 93,580 | 77,184 | 16,396 | 3,806 | 1,582 | 27.4 |
| Search-first worker prompt | 6 | 66,105 | 49,280 | 16,825 | 3,849 | 2,198 | 29.9 |

Both identified the two validation gates and unconditional effort emission.
The revised prompt reduced repeated input by 29%, but uncached input, output,
reasoning and elapsed time did not improve. This one uncontrolled pair does not
establish quota savings. Input totals span all model requests; reasoning is a
reported detail, not an additional output-token charge to add to the total.
Exact-provider wire acceptance still needs to verify any proposed thinking-off fix.

Validation of this finishing slice: 20 focused configuration/CLI checks passed;
the caller independently reran the nine `config_cli` tests. Formatting and
`cargo clippy --locked --workspace --all-targets --all-features -j 2 -- -D warnings`
passed. This was not a full test-suite run. Windows emitted non-fatal incremental
cache-finalization access warnings on V:, which may reduce later cache reuse.

## LCM and round policy (2026-10-17)

The current example has no default round ceiling and explicitly selects a 200k
root physical context cap and a 100k child cap, including the configured response
reserve (16k by default). Root/native/hidden workers gather episodes
as source work grows and keep the serving prefix frozen until apply. Episode,
chapter, evidence and serving allocations are configurable in `[context.lcm]`.
See the approved [LCM contract](lcm-contract.md) and
LCM worker record in the project notes for implementation and verification.
The live smoke results above are historical and predate this change.
The launcher writes a separate metadata-only trace under the worker data directory's
`traces/` folder for each invocation; `-TraceFile` selects another path. This records
LCM operation modes and failures without enabling prompt/tool-content tracing.

## Remaining work

- Built-in child profiles still inherit the parent's model. `read-only` and
  `workspace` and web `research` inherit the optional root round limit; the
  default is unlimited. Named configurable child profiles would let one
  host route different worker jobs without launching separate processes.
- LCM source, evidence, summary and serving allocations are now configurable in
  `[context.lcm]`; the local worker example uses 200k root / 100k child physical caps.
  Benchmark continuation quality and actual cached/uncached usage before choosing
  more aggressive allocations.
- Chat reasoning currently requires enabled/adaptive thinking; permit supported
  non-thinking requests with correct provider-specific wire fields.
- A provider reply still cut after its continuation allowance currently warns
  and returns partial text. Propagate an explicit incomplete result before relying
  on unattended workers for tasks where truncation could look like completion.

Keep protocol validation, authoritative history, workspace identity and unknown
effect handling intact while exposing these policy choices.
