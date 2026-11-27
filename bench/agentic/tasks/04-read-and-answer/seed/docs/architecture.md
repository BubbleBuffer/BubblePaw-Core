# Service Client Architecture

> Note: Some figures below reflect the original 2.0 design. Check `configs/` and `src/` for current values.

The outbound HTTP client (`src/client/http.ts`) wraps `fetch` with:
1. Per-attempt request timeout (originally 3000ms).
2. Exponential backoff with full jitter (`src/client/retry.ts`).
3. A per-host circuit breaker (`src/client/breaker.ts`) that opens after consecutive transport/5xx failures.

Configuration is loaded by `src/config/load.ts`: `configs/default.json` is merged with `configs/<env>.json`.
