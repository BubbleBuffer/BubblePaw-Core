# Delegation (`bubblepaw delegate`)

Status: built 2026-10-19 (v0; see [Current state](#current-state-2026-10-19)).

## Goal

A dispatcher (a person, a script or another agent) hands well-defined work to BubblePaw, usually on cheaper models. Each delegation also becomes an eval sample, so choosing a model is a number that updates from ordinary work, without a separate benchmark to maintain. Success metric: usage per completed outcome, including rework.

## Shape

`bubblepaw delegate` is a runner subcommand on top of the existing one-shot worker run ([local-worker](local-worker.md)). It takes a workspace and an assignment file. It prints the JSONL events, then one compact result record. It is not an MCP server and not a plugin platform. Delegation logic lives in the runner, outside the engine (AGENTS.md).

Assignment file: Markdown with TOML front matter.

- `category`: required, from a fixed list in code ( implement, fix, refactor, test-write, test-run, research, review, docs, migration, ops). Later automatic routing is based on it, so the list is controlled, not free text. An unknown category is rejected.
- `tags`: optional free labels, such as language or area.
- `checks`: acceptance commands, run in the workspace after the worker stops. Each is expected to exit 0. The dispatcher writes them, for example `cargo test -p x`.
- `rework`: number of resumes with failing check output (default 1).
- `route`: exactly one of `model = "provider/model"` (sensitive tasks) or `class = "small" | "medium" | "large"`. No default: the dispatcher always states it.
- `parent`: optional ledger id of an earlier failed attempt, with `reason = "reframe" | "escalate"`, when this assignment follows up a failed resolution.
- `read_first`: optional list of workspace paths, each optionally `path:start-end` (1-based, inclusive lines). The files are read when the run starts, before the session or any model call, and their contents go into the first user message after the body, under `## Read first`. A missing file, a path that is absolute or leaves the workspace (also through a symlink), a malformed or out-of-bounds range, a non-UTF-8 file, or more than 64 KiB in total rejects the assignment, naming the entry. Rework resumes do not repeat the contents. The contents count toward the assignment length; the hash covers the assignment file only.

Model classes (config `delegate.classes`, one default and the other candidates per class; list a model only once BubblePaw can reach it). This is the only delegate config; the explore share, category list, rework default, check output tail and price table are constants in code. Classes are a cost and capability ladder: `small` for cheap models that handle well-defined pieces, `medium` for goal-sized work with settled decisions, `large` for open design or work a medium model failed.

Sensitive tasks: security, data deletion, migrations, public APIs, or anything the dispatcher marks. They name a model and are never explored.

## Pieces

1. **Harness-run checks.** After the worker finishes, BubblePaw runs `checks` itself and records exit code, duration and a bounded output tail. On failure it resumes the same session with that output, up to `rework` times. If the checks still fail, it stops and returns a failed resolution. That result gives the failing checks, the worker's report and its open questions. BubblePaw does not escalate by itself. The dispatcher decides whether to reframe the assignment (same class, rewritten) or escalate (a larger class or a named model), and sends a new assignment with `parent` set. The worker's own claims never decide acceptance.
2. **Structured final report.** The worker finishes by calling `submit_result` with a schema-validated report (`RunOptions.output_schema` supplies the tool input schema): `status` (done, failed, blocked), `summary`, `checks_claimed` (command, passed), `open_questions`.
3. **False-green detection without a judge.** A false green is counted when a claimed-passing check fails the harness run, or when the claim names a command whose last recorded exec in the session exited non-zero. Both come from recorded facts, not text judgement.
4. **Outcome ledger.** One SQLite file, `delegations.db` beside the store, holds one row per delegation attempt:
   - assignment hash, category and workspace;
   - route, and the routing reason (named, default, explore, reframe, escalate);
   - check results and false-green count;
   - rework index;
   - usage by token class, and list-price cost from a dated price table;
   - wall time, session id and final status.

   One transaction per attempt. `bubblepaw delegations report` summarizes cost per accepted outcome by category, class and model.
5. **Explicit routing with in-class exploration.** A `model` route runs that model; it is recorded as `named`. A `class` route runs that class's default, except for an explore share of 0.15 (a constant). An explored attempt goes to a uniformly chosen other candidate of the same class, and is recorded as `explore`. The result record names the model and the reason, so the dispatcher always sees what ran. Follow-up attempts record `reframe` or `escalate` and link to their parent. Escalation data is kept apart from explore data, because it only covers tasks the first model failed.
6. **Metrics.** Everything below is stored with the attempt in the same transaction. Nothing here is used for acceptance.
   - **Dispatch features**, recorded before the run for later automatic routing: category, tags, repo, assignment length in tokens, number of checks, class or named model, and peak or off-peak.
   - **Per model call** (`delegation_calls` rows):
     - model, provider route, account and fallback reason;
     - uncached input, cache read, cache write, output and reasoning tokens;
     - time to first token, generation time, output tokens per second;
     - outcome: ok, retried, rate-limited, timed out, stream error or malformed.
   - **Per attempt**:
     - cache hit ratio, as cache read over all input;
     - median and p90 tokens per second;
     - wall time, split into model time, tool time and check time;
     - call count, and tool calls by tool name;
     - tool errors and malformed tool calls;
     - repeated identical calls;
     - claimed done with no check run;
     - tokens and time to the first passing check;
     - LCM gathers, applies and rejected summaries;
     - files changed and lines added or removed;
     - rework index.
   - **Reliability**, reported per model and route: provider error rate, retries, fallbacks, rate-limit hits, timeouts, unknown-outcome effects, and attempts that ended without a final report.

   `bubblepaw delegations report` groups these by category, class and model.
7. **Compact result to the dispatcher.** The result record carries:
   - status;
   - the verified check lines;
   - changed files and diff stat;
   - the bounded summary;
   - cost and the ledger id.

   The dispatcher reads this record instead of re-checking the worker's claims.

## Reframe or escalate

A failed resolution is the dispatcher's call:

- **Reframe** (same class) when the failure shows the assignment was unclear, wrong or too big: a misread goal, a missing fact, or work outside its scope.
- **Escalate** one class when the worker understood the task but couldn't do it, or after one reframe already failed.
- **Never** retry the same assignment unchanged.

## Out of scope for v0

- Remote execution of the worker itself (checks may reach another host through their own commands).
- Model judges.
- Automatic default changes (the dispatcher or user changes `delegate.classes` from the report).
- Headroom-based subscription account choice.
- Replay/fork evals.

## Open questions

- Whether fixed benchmark cases should feed the same ledger as a rarely-run smoke test.

## Current state (2026-10-19)

- 2026-10-21: concurrent `delegate` processes attach to one store owner. Each keeps its workspace and resolved config; checks, reworks and ledger writes run in the host, with one transaction per attempt. Referenced MCP environment credentials are scoped to each request's servers. See [host attach](host-attach.md) for protocol, cancellation and verification status.
- `bubblepaw delegate <assignment> [--seed N] [--name T]` runs in the runner ([`delegate.rs`](../crates/bubblepaw-runner/src/delegate.rs) and [`delegate/`](../crates/bubblepaw-runner/src/delegate/)). The engine is unchanged; `lib.rs` only declares the module.
- The route is chosen before the provider is built, by pushing `--set model.main=<model>` into the normal config load. `--seed` makes the explore draw reproducible.
- The worker calls `submit_result` with the schema-validated report; `Run.text` retains its JSON for report parsing. Providers receive no response schema. If the run completes, the harness runs each check through the same shell as `exec` (`sh -c`, or PowerShell on Windows) and keeps a 2048-byte output tail. Failing checks resume the same session, up to `rework` times.
- Final status: `accepted` (all checks pass), `failed` (checks still fail after rework, the run did not complete, or no valid report), or `unchecked` (the assignment lists no checks). Exit code 1 on `failed`.
- A run that does not complete (provider error, crash, cancellation) is not reworked and its checks are not run. The result is a failed resolution whose `error` names the cause, and the dispatcher decides what follows (decided 2026-10-19).
- Each check is killed after 30 minutes (a constant). A timeout is a failed check, recorded with `timed_out` and the line `fail (timeout)`.
- An assignment without checks ends `unchecked`, since research tasks have none (decided 2026-10-19).
- False greens come from the harness check results and from exec commands and exit codes seen on the live event stream (including yielded jobs resolved through `poll`/`wait`). Nested exec commands are not durable in the store, so the runner observes the existing transient events.
- Per-call rows come from the existing observation records (`model_started`/`model_finished`/`model_failed`), collected by an in-memory sink that still forwards to the user's trace when one is configured.
- `delegations.db` beside the store holds `delegation_attempts` (one row per attempt) and `delegation_calls`; each attempt is one transaction, checked in a test by counting WAL commit frames. The ledger keeps one numbered `user_version` step: step 1 adds the cache columns introduced after the first release, in one transaction with its version bump, so a first-release ledger opens with its rows and a current ledger inspects no table. `delegation_id` groups the attempts of one invocation; the result's `ledger_id` is the last attempt's id and is what a follow-up names as `parent`.
- Cost uses the runner's shared embedded price table (`src/prices.json`, checked 2026-10-19), also used by native usage reports. An unpriced model or token class gives a NULL cost; the report then omits cost per accepted outcome for that group.
- `bubblepaw delegations report` prints JSON groups by category, class (`named` for model routes), model and routing reason, so escalation data stays apart from explore data.
- Stored as NULL because nothing records them today: account, retries, rate-limit hits, timeouts (failures only distinguish cancelled from provider error), LCM applies and rejected summaries, and the retried, rate-limited, timed-out and malformed call outcomes. Time to first token is missing for responses with no text. The provider route is per attempt, from the assistant messages' route origin.
- Tokens and time to the first passing check are measured within each attempt.
- Repeated identical calls ignore `poll`, `wait` and `jobs`.
- 2026-10-21: `read_first` is implemented ([`read_first.rs`](../crates/bubblepaw-runner/src/delegate/read_first.rs)). Syntax is checked at parse; files are read at the start of `delegate::run`, which `main.rs` calls after it builds the harness. Reading before provider setup needs a `main.rs` change, not made yet. No live model run yet.
- 2026-10-22: workers are told never to use `git stash`. A linked worktree shares its stash with the repo's other worktrees, so parallel runs popped each other's work; a worker sets work aside with `git diff` or a temporary `git worktree add --detach` ([`prompts.rs`](../crates/bubblepaw-runner/src/delegate/prompts.rs)).
- Tests: assignment parsing including malformed input, routing with a seeded RNG, pricing, false-green rules, metrics, and scripted-model integration tests for rework-then-pass, exhausted rework, the result record shape and the CLI (`tests/delegate.rs`). No live model run yet.
