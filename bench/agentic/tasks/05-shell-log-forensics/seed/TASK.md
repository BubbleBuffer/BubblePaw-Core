# Log Forensics Task

Analyze every `*.jsonl` file under `data/` and write `report.json` in the workspace root.

## Input Format
Each non-empty line in `data/*.jsonl` is expected to be a JSON object with:
- `event_id` (string)
- `service` (string)
- `status` (number)
- `duration_ms` (number)
- `trace_id` (string)
- `error_code` (optional string)

Blank lines are ignored and do not count toward `total_lines`.

## Rules
1. **Malformed lines**: Any non-empty line that is not valid JSON or is missing a valid `event_id`, `service`, `status`, `duration_ms`, or `trace_id` counts in `malformed_lines` (and `total_lines`), and is otherwise skipped.
2. **Deduplication**: Deduplicate valid events by `event_id` (first occurrence kept; subsequent occurrences count in `duplicate_events` and are excluded from service/error metrics).
3. **Errors**: An event is an error if `status >= 500`.
   - `failed_trace_ids`: sorted (lexicographic ascending) array of unique `trace_id` values from error events.
   - `error_counts`: map of `error_code -> count` across unique error events that include a non-empty `error_code` string.
4. **Per-service metrics** (`services[service]`):
   - `count`: number of unique events for that service
   - `error_count`: number of unique error events (`status >= 500`) for that service
   - `p95_duration_ms`: nearest-rank 95th percentile of `duration_ms`: sort durations ascending and pick `sorted[Math.ceil(0.95 * n) - 1]`.

## Output Schema (`report.json`)
```json
{
  "total_lines": 0,
  "malformed_lines": 0,
  "unique_events": 0,
  "duplicate_events": 0,
  "failed_trace_ids": [],
  "error_counts": {},
  "services": {
    "service_name": {
      "count": 0,
      "error_count": 0,
      "p95_duration_ms": 0
    }
  }
}
```
