# ADR-001: Outbound Transport Timeouts

Status: Accepted

Each outbound attempt uses an `AbortController` bounded by `timeoutMs`.
The initial global default in `configs/default.json` is `3000` ms, overridable per environment.
