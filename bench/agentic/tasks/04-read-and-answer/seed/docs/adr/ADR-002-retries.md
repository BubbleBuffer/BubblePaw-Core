# ADR-002: Initial Retry Policy

Status: Superseded by ADR-004

Initial parameters chosen for v2.2:
- `maxRetries`: 3 (4 total attempts)
- `backoffBaseMs`: 200
- `backoffCapMs`: 3000
- `retryableStatuses`: `[429, 502, 503]`
