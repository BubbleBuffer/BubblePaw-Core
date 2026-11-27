# ADR-004: Revised Production Backoff and Retryable Statuses

Status: Accepted
Supersedes: ADR-002

Following the 2026-07-04 gateway storm:
1. Production uses `timeoutMs = 2500` so slow hung sockets fail faster.
2. Production uses `maxRetries = 4` (5 total attempts with the loop in `src/client/retry.ts`).
3. Backoff uses `backoffBaseMs = 120` and `backoffCapMs = 4800`.
4. `retryableStatuses` expands to `[408, 429, 502, 503, 504]`.
5. Production `breakerOpenMs` is set to `45000`.
