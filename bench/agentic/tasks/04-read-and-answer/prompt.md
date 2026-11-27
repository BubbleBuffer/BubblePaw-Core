Read the docs, ADRs, configs, changelog, and source code in this repository to determine the **actual effective behavior in `prod`** (not historical defaults or superseded proposals).

Create a new file `ANSWER.json` in the workspace root with these exact 7 keys:

```json
{
  "prod_timeout_ms": <number>,
  "prod_max_total_attempts": <number>,
  "prod_backoff_base_ms": <number>,
  "prod_backoff_cap_ms": <number>,
  "prod_breaker_open_ms": <number>,
  "retryable_statuses": <number[] sorted ascending>,
  "superseding_adr": <string, e.g. "ADR-00X">
}
```

Constraint: **Do not modify, rename, or delete any existing file** in the repository. Only add `ANSWER.json`.
