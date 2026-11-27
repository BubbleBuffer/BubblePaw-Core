# Job Queue v2 Upgrade Specification

Upgrade the existing v1 FIFO queue in `src/` to v2 with the following contracts:

## 1. Types (`src/types.ts`)
- `Priority = "high" | "normal" | "low"` (default `"normal"`).
- `JobStatus = "pending" | "done" | "dead" | "blocked"`.
- `AttemptRecord = { attempt: number; ok: boolean; error?: string }`.
- `EnqueueOptions`:
  - `id: string`
  - `payload: unknown`
  - `priority?: Priority`
  - `dependsOn?: string[]`
  - `maxAttempts?: number` (integer `>= 1`, default `1`)
- `JobRecord`: retains `id`, `seq`, `payload`, `status`, plus `priority`, `dependsOn`, `maxAttempts`, `attempts`, `attemptHistory`, optional `lastError`, optional `blockedBy`.

## 2. Persistence (`src/storage.ts`)
- `loadQueueState(filePath)` returns `{ nextSeq: 0, jobs: [] }` if the file does not exist.
- `saveQueueState(filePath, state)` writes atomically via a temp file in the same directory and `rename` (creating parent directories if needed).

## 3. Queue (`src/queue.ts`)
- Export `class CycleError extends Error` with `readonly cycle: string[]`.
- `TaskQueue.open(filePath)` and `queue.save(filePath)`.
- `enqueue(options)`:
  - Throws if `id` already exists or `maxAttempts` is not an integer `>= 1`.
  - Validates `dependsOn`:
    - If `depId === options.id` or adding dependencies creates a cycle, throw `CycleError` **without mutating queue state**.
    - If `depId` does not exist in the queue, throw an `Error` **without mutating queue state**.
  - If any dependency is already `"dead"` or `"blocked"`, the newly enqueued job starts with `status: "blocked"` (and `blockedBy` set to that dependency's ID).
- `listRunnable()` / `nextRunnable()`:
  - A job is runnable iff `status === "pending"` and every job in `dependsOn` has `status === "done"`.
  - Order runnable jobs by priority (`high` before `normal` before `low`), breaking ties by `seq` ascending (FIFO within each priority).
- `markDone(id)`: increments `attempts`, appends `{ attempt, ok: true }` to `attemptHistory`, sets `status = "done"`.
- `markFailed(id, errorMessage)`: increments `attempts`, sets `lastError`, appends `{ attempt, ok: false, error: errorMessage }`. If `attempts >= maxAttempts`, sets `status = "dead"` and **transitively** marks every `"pending"` job that depends directly or indirectly on `id` as `status = "blocked"` (with `blockedBy` set).
- `statusSummary()`: returns `{ total, pending, done, dead, blocked }`.

## 4. Worker (`src/worker.ts`)
- `runWorker(queue, handler)` repeatedly picks `nextRunnable()`, executes `await handler(job)`, calls `markDone` or `markFailed`, and returns `{ executedIds, doneIds, deadIds, blockedIds }`.
  - `executedIds`: list of job IDs in exact execution order (may contain the same ID multiple times across retries).
  - `doneIds`: IDs of jobs completed during this `runWorker` call.
  - `deadIds`: IDs of jobs that transitioned to `"dead"` during this `runWorker` call.
  - `blockedIds`: IDs of all jobs in the queue currently in `"blocked"` state.

## 5. CLI (`src/cli.ts`)
Support:
- `enqueue --state <path> --id <id> [--payload <json>] [--priority high|normal|low] [--depends-on a,b] [--max-attempts n]` -> prints JSON of the enqueued `JobRecord`.
- `run --state <path> [--mode succeed|fail-ids|fail-all] [--fail-ids a,b]` -> runs `runWorker`, saves state, prints JSON `WorkerSummary`.
- `status --state <path>` -> prints JSON `{ summary: queue.statusSummary(), jobs: queue.list() }`.
