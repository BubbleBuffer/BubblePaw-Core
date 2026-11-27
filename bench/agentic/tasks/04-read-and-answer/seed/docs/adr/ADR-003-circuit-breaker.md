# ADR-003: Circuit Breaker Open Window

Status: Accepted

The circuit breaker tracks consecutive retry-exhausted failures per target host.
Default `breakerOpenMs` is `30000`. Production overrides this in `configs/prod.json` when upstream recovery takes longer.
