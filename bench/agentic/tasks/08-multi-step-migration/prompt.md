Upgrade the v1 job queue library (`src/types.ts`, `src/storage.ts`, `src/queue.ts`, `src/worker.ts`, `src/cli.ts`) and tests (`tests/queue.test.ts`) to match `SPEC.md`.

Keep existing FIFO behavior working for default jobs, add full support for priority lanes, dependency DAGs with cycle detection, retries with dead-letter and transitive blocked cascade, atomic persistence, and the new CLI flags. Verify with `bun test` and CLI smoke checks.
