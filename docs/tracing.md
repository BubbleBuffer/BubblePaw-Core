# Experimental behavior tracing

Status: experimental, optional, off by default.

Tracing is optional and disabled by default. It is a disposable observation stream,
not session history, an effect ledger or a source for replay. No tracing records
are added to the canonical or serving SQLite databases. The core exposes a small
optional observer; the runner writes JSONL through a bounded queue on a dedicated
thread. Provider token fragments do not generate individual trace writes.

Operational warnings from the core and runner go to stderr through `tracing`. The runner binary shows `warn` and above by default; set `RUST_LOG` (for example `RUST_LOG=debug`) to change the filter. Library embedders install their own subscriber, and without one these warnings are dropped.

## Enable and inspect

Set `BUBBLEPAW_PROVIDER_DUMP_DIR` to a local directory to opt into paired `<timestamp>-<request kind>-<seq>.request.json` / `.response.txt` files containing provider HTTP bodies (including raw SSE and readable reasoning), without HTTP headers (cancellation and existing read limits can leave a partial response); dumping is off by default, and empty summary errors include at most 500 characters of readable reasoning.

```powershell
.\target\release\bubblepaw.exe --workspace . --trace-file .run\behavior.jsonl --trace-label cmd-v0 web --port 8788
```

`--trace-mode metadata` is the default. It records identifiers, byte/count metrics,
fingerprints, timing, actual provider usage and failure categories without raw
prompts, code, arguments or results. Native program basenames and conservative
bare first tokens of shell scripts are included for frequency counts. For deeper local investigation, add
`--trace-mode contents`: bounded prepared prompts, responses, code, command
templates and tool results are included. Provider continuation state and
transport authentication are excluded. Content may contain private workspace
information; this mode is an explicit opt-in, not general secret sanitization.

Relative trace paths resolve inside the workspace. `--trace-label` identifies a
configuration; startup records include package version, a fingerprint of selected
compiled source files, model/provider/platform and trace limits. The fingerprint's
scope is recorded; it is not a whole-repository or frontend build identity.

```powershell
.\target\release\bubblepaw.exe trace-report .run\behavior.jsonl
```

The report reads the current file and its retained rotations without contacting
a model. It summarizes tool usage, native calls associated with JS executions,
raw versus tagged command attempts, exact repeated inputs, failures and provider
usage. Exact repeats are observations, not automatically classified as mistakes;
polling a process is a legitimate repeated operation. Command fingerprints count
exact inputs, not a full semantic parse of every command inside a shell script.
`program_attempts` names literal native programs; `script_prefixes` only names a
script's first bare token. `completed_program_calls` means the host call returned
without a tool error, not that the process exited successfully. Execution failures,
steering/followup inputs and cancellation intentions are counted separately.
Process metrics include exit codes, timeout/cancellation status, capture byte
counts and gaps. Process-exit totals count result observations; repeated polling
can report the same process exit more than once.

## Prompt-cache events

`context_built` (one per built request: tier fingerprints, classification,
declaration, and since P8 the lineage `seq` and `since_previous_ms`) and
`cache_miss_unexplained` (a request whose cached tokens fall short of the
predicted unchanged prefix, or an undeclared non-append change; it carries the
expected and actual cached tokens, the gap and `likely_expired`) are described
in [prefix-cache-design](prefix-cache-design.md#request-log-and-unexplained-cache-misses-p8-2026-10-21).
The per-request rows behind the warning are kept in the serving store whether or
not tracing is on; they are disposable telemetry, not trace records. Each row
also carries cache writes, output and reasoning tokens, the requested reasoning
effort, the sent cache breakpoints, the cache lifetime and `likely_expired`, so
real traces can be analysed offline from serving.db alone.

## Correlation and bounds

Records correlate session, run, model request, call, invocation and, for nested JS
calls, environment/execution/generation. This permits offline analysis of which
tools a cell composed and how that behavior changes between labeled builds.

The default queue holds 256 records; batches contain up to 64. Each record is
bounded to 64 KiB, with at most 32 KiB of content preview. Payload previews are
text and explicitly identify truncation; the enclosing JSONL record remains
valid JSON. Files rotate at 32 MiB with three files retained in total. Change the
per-file bound using `--trace-max-mib` (1 through 1024).

Queue overflow drops observations instead of blocking execution. Drop counts,
writer errors and flush status are observable, so traces are never assumed to be
complete. Shutdown requests a bounded flush; an interrupted process may lose
queued observations. Use a different trace path for each concurrently running
host: v0 assumes one writer per path. These files can be deleted independently of sessions.

This is a first collection boundary, not an experiment scheduler, automatic
behavior classifier, dashboard or stable telemetry schema. Add new metrics where
they answer a concrete question; preserve the disabled path's minimal overhead.
