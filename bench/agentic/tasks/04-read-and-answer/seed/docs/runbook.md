# On-Call Runbook: Outbound Retries

- Staging uses `configs/staging.json` (`timeoutMs = 4000`, `breakerOpenMs = 20000`).
- Production uses `configs/prod.json`.
- Remember that `maxRetries` in config counts **retries after the first call**; verify `src/client/retry.ts` to confirm total attempts.
- See `docs/adr/` for the active retry and breaker policies.
