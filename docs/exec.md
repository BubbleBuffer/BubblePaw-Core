# Command execution: sync and background jobs

**Status:** built 2026-10-13; idle completion wake and Windows exec ownership added 2026-10-14, design approved 2026-10-13. Scripted and local
acceptance only; no live model acceptance.

Commands are the native capability `tools.exec` in `crates/bubblepaw-runner/src/tools/process.rs`,
reached through the `js` model tool. A "job" is the existing `process:<uuid>` handle, routed by
`tools.poll`, `tools.jobs`, `tools.cancel` and the `wait` helper. There is no second job system.

## Model-facing behaviour

- **Sync by default.** `tools.exec({command, cwd?, workdir?, background?, yieldMs?, timeoutMs?})` blocks
  until the command exits, up to `exec.sync_timeout_secs` (default 300). CodeMode classifies the
  blocking part as an explicit wait, so the 30 s ordinary host-call deadline does not cut it off;
  cancellation still applies. Explicit `yieldMs` overrides this foreground window; an explicit
  `timeoutMs` can extend it beyond the configured default and remains a hard deadline.
- **Agent backgrounding.** `background:true` (or `yieldMs:0`) returns at once with
  `status:"running"` and the handle. `yieldMs` is an explicit foreground window and can exceed the configured sync timeout.
- **Backgrounded result.** A sync call that stops waiting returns `status:"backgrounded"` with
  `reason` `sync_timeout`, `user` or `steer`, the handle, output so far and cursors. The command
  keeps running. `timeoutMs` keeps its meaning: a hard deadline that kills (`timed_out`).
- **Job tools.** `tools.jobs()` lists the session's handles with status, exit code, stdout/stderr
  byte counts and age. `tools.poll({handle})` without a cursor reads output since this session's
  last cursorless poll (server-held cursor); explicit cursors stay independent. Replies keep the
  16 KiB-per-stream preview bound. A completed command with more than
  64 KiB of captured output also returns `fullOutput:{path,bytes,format:"text"}`
  when its spool is complete. `wait({handle, timeoutMs})` and `tools.cancel({handle})` are
  unchanged.
- **Yielding polls.** Positive `yieldMs` waits for completion up to 30 seconds
  and suspends CodeMode's non-wait deadline. Omitted or zero `yieldMs` inspects
  immediately. Use `wait({handle})` for a completion wait, then inspect output
  with `poll`. Ordinary steering stays queued; explicit background and
  cancellation controls remain available.
- **Directory note.** Each exec starts in its project dir (or `cwd`), so a `cd` does not carry over. When a shell command ends in another directory, its result (the sync result, or the final `wait`/`poll`) gets `note: "Working directory reset to <start dir>; each exec starts there. Pass `cwd` to run elsewhere."`. POSIX uses an exit trap with final `pwd -P`; Windows PowerShell uses `try/finally` with the final FileSystem provider location. Both write a spool marker only when the final folder differs from the start folder. The Windows wrapper saves the command's success status before marker housekeeping and preserves explicit `exit` codes, stdout, stderr and command error text. `powershell.exe -NoProfile -NonInteractive -Command` remains the default Windows shell: a final failed native command maps to exit 1 unless the script explicitly uses `exit $LASTEXITCODE`. Killed shells, POSIX `exec` replacement or a user replacement EXIT trap, native argv commands and hosts without a spool directory get no note.
- The JS tool reference text describes all of this (`js_reference` in `codemode.rs`).

## User backgrounding

- **Explicit:** `POST /api/sessions/{id}/processes/{handle}/background` in the web host, stdio
  methods `process.background {session_id, handle}` and `process.list {session_id}`, and a
  "Background" button on a running command in the web view. The blocked call returns
  `reason:"user"`.
- **Steer (2026-10-18):** ordinary steering is durably queued without releasing
  foreground waits or cancelling work. It joins the next main request after the
  current tool finishes normally. Hosts can separately call the explicit
  `ToolRegistry::interrupt_waits` control to release waits without cancelling
  their work. That legacy release returns `reason:"steer"` for process waits;
  exec-only CodeMode cells finish normally, while mixed explicit waits can detach.
  Explicit user backgrounding and cancellation keep their existing behavior.

## Completion notices

When a backgrounded job ends, the process tool queues one line for the session:
`Job <handle> <status> (exit code <n|none>); stdout <n> bytes, stderr <n> bytes.` Jobs that were
never backgrounded (a sync call received the result) get no notice.

Delivery uses a small core hook: `Tool::take_reminders(session) -> Vec<Reminder>` (process
reminders are built in `process_reminders.rs`), gathered by `ToolRegistry` over model and host
tools. Before each model request, the turn loop queues them and flushes the pending ones as one
durable user-role message with `origin = Generated`, one `<harness kind="…">…</harness>` entry
per reminder, committed with its `reminder_metadata`, then places it at the end of that request.
So completions arrive at the next model call of an active turn, exactly once, and the record
holds exactly what the model saw. `latest_user_request` skips these messages.

Long-lived web and stdio hosts automatically start a new turn when the session is idle.
Child and exec completions share a debounced batch: `agents.auto_wake=true` by default,
with `agents.wake_debounce_ms=1500`. The wake input uses the same durable generated
reminder format and the ordinary event stream. The web transcript labels generated
`<harness …>` reminders and legacy `[host notice]` input as “Host notice.”
Turning auto-wake off preserves next-request delivery.

Explicit stop/cancel suppresses wakes until accepted user text. Archived, closed and
hidden sessions do not wake. Hidden workers never wake callers. Known terminal notice
metadata is kept as a small immutable session attachment, `bubblepaw-process-notice-v1:<handle>`,
with the job handle and text; its delivery identity is `process_completion` plus the handle.
Undelivered ones are recovered from these attachments only at the next model request after restart. Scheduling does not survive a restart, and
interrupted commands with unknown outcomes are never replayed or claimed complete.
See [child completion wake](child-sessions.md#completion-notices-and-idle-wake) for
runner-library host registration and lifecycle.

Running children use the same hook for [stale-child reminders](child-sessions.md#stale-child-reminders).
The runner computes inactivity and repeat intervals; core only delivers the notice.

## Storage

- Live output is spooled to `<data_dir>/jobs/<host-id>/`, capped per job by
  `exec.max_output_bytes` (default 8 MiB; beyond it only head and tail are kept and gaps are
  recorded as `spoolGaps`). In-RAM head/tail previews for replies are unchanged.
- Before a completed large job is reaped, complete captured text is copied to
  `<data_dir>/spill/<chat-id>/<id>.txt`. It contains one metadata line after
  `[process result]`, then `[stdout]` and `[stderr]` sections with the decoded
  streams. The absolute reference remains available after the spool is removed
  and supports ordinary `tools.read` line ranges. Repeated terminal polls reuse
  the same saved reference. `chat-id` is the owning session ID, with a separate
  folder and allowance for each child session. The 256 MiB per-session spill cap
  evicts oldest files first; chat deletion removes these files and archiving retains them.
- Spilling cannot recover bytes already dropped by `exec.max_output_bytes` or a
  pipe capture error. Such a large result returns `fullOutputError` and its
  existing capture gaps; it never calls a partial spool complete. Invalid UTF-8
  remains explicitly lossy in metadata. Raw binary preservation still requires
  redirecting the command to a file.
- Each host process owns one spool directory with a held file lock (`.lock`). A spool is removed
  when its job is reaped and on session cancel; at startup a host sweeps only spool directories
  whose lock it can take, so several hosts may share a data directory. Sweeping and creating
  a spool hold `jobs/.sweep.lock`, so a sweep never removes a spool that is not yet locked.
- Nothing per line or per chunk reaches SQLite. Measured on Linux with a 4 MB background job:
  messages stayed at 4 rows, effects at 2 rows, the chat database at 4 KiB; the completion
  transaction grew the WAL by 4,120 bytes.

The 2026-10-14 wake fixture on Linux streams 2,097,152 exec bytes and 1,000
model fragments. Known completion metadata plus the automatic parent response
add two canonical messages and four application transactions, with 210,120 bytes
of WAL growth and 12,288 database bytes. This invokes exec directly as a host
fixture; counts exclude a model's launch tool-dispatch effect. It verifies bounded
writes rather than production performance.

The runner and `register_native_tools_with_exec` configure this data-directory
spool and spill store. The legacy `ProcessTools::new` / `register_native_tools`
constructors have no data directory and retain their bounded RAM-only capture;
they cannot preserve stream middles already dropped from RAM.

## Effects, approvals, lifetime and limits

- Starting a command is an external effect with a `dispatched` row before spawn. After a host
  restart such a job is `unknown`, `tools.jobs()` shows nothing live, and nothing is re-run.
- Approval is unchanged (`exec` declares `Approval::None`; per-tool policy applies) and happens
  before the start; backgrounding never touches it.
- Jobs belong to the host process. Every exec runs in its own process tree by way of
  `bubblepaw-process`: a process group on Unix, a non-inheritable Job Object with
  kill-on-job-close on Windows (the child starts suspended, joins the job and only then
  resumes, so its descendants inherit membership before they can run). The whole tree is
  killed on cancellation, timeout, host shutdown or dropping the host owner, and also when
  the session ends normally. Normal shell exit kills the tree too, before output draining,
  ending any remaining descendants. This ownership covers both shell and native argv
  commands. Assignment or resume failures terminate the child and fail the launch. Abrupt
  host death between suspended creation and assignment can leave a suspended child that has
  never run; it cannot spawn grandchildren. This exec lifecycle is separate from
  [MCP](mcp.md).
- `exec.max_jobs_per_session` and `exec.max_jobs` are optional caps, unset (unlimited) by default;
  the old fixed 32-process limit is gone. A set cap rejects the start before spawn with a
  correctable error; the effect row is already written at that point and settles with that
  error result (moving the check before the effect row would need dispatcher admission in core).

## Config

```toml
[exec]
sync_timeout_secs = 300      # sync calls move to background after this; 0 = background at once
# max_jobs_per_session = 4   # optional cap; unset = no limit
# max_jobs = 16              # optional host-wide cap; unset = no limit
max_output_bytes = 8388608   # per-job spool cap (head and tail kept)
```

`ProcessTools::apply_config` replaces the settings for the next exec call. Each
call snapshots admission caps, foreground timeout and output spool cap together;
running jobs retain their original foreground wait, hard timeout and spool budget.
A lower cap rejects new starts while existing jobs continue, and raising or clearing
it admits subsequent jobs.

See [config](config.md#exec).

## Tests

Scripted/local only: `crates/bubblepaw-core/tests/host_notices.rs` (notice delivered once, stored
once, request placement), `crates/bubblepaw-runner/tests/exec_jobs.rs` (sync, sync timeout,
background, user/steer backgrounding, jobs, cursorless poll, caps, spool cap and sweep, shared
data dir, row counts, cwd notes on Unix and Windows, Windows child/grandchild cleanup on cancel,
hard timeout and natural shell exit, preserved Windows exit codes and error text),
`exec_codemode.rs` (deadline escape, steer inside a cell), and
`web_processes.rs` (HTTP route). `auto_wake.rs` exercises shared child/exec batching,
idle/busy delivery, restart notice recovery and concurrent steering/completion;
`web_wake.rs` verifies automatic turns through HTTP/SSE, and `wake_storage.rs`
bounds SQLite writes while exec output and model fragments stream.

## Defaults and argument leniency (2026-10-20)

The per-stream preview is 16 KiB. `await wait({handle})` uses a 600-second
completion window; `timeoutMs` may raise or lower it, including zero for immediate
inspection. A wait timeout returns `status:"timeout"` and leaves the process
running. Explicit waits suspend CodeMode's idle deadline. Bare UUIDs are accepted
by poll, wait and cancel; replies retain canonical `process:<uuid>` handles and
conversation ownership checks. `workdir` aliases `cwd`; when both are supplied
they must resolve to the same directory.