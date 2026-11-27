# Changelog

## 2.4.0 (2026-08-19)
- Implemented ADR-004 in `configs/prod.json` and `src/client/retry.ts`:
  - Lowered prod `timeoutMs` from 3000 to 2500.
  - Raised prod `maxRetries` from 3 to 4 (5 total attempts including initial request).
  - Updated backoff base to 120ms and cap to 4800ms.
  - Added 408 and 504 to prod `retryableStatuses`.
  - Extended prod circuit breaker open window to 45000ms following July incident.

## 2.2.0 (2026-06-12)
- Added staging overrides (`configs/staging.json`).
- Initial ADR-002 retry rollout: `maxRetries: 3`, `backoffBaseMs: 200`, `backoffCapMs: 3000`.

## 2.0.0 (2026-04-01)
- Initial HTTP client with default timeout 3000ms and 30000ms circuit breaker.
