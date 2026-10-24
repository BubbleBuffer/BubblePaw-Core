# BubblePaw base

Build a usable, small Pi-style Rust harness. The library is primary; the runner
is a thin consumer. Keep BubblePaw product logic, child orchestration, MCP and
frontend projections outside the engine. Do not add a plugin platform or context
experimentation framework to v0.

The authoritative store is SQLite. Persist accepted input and complete messages,
plus necessary effect boundaries. Stream fragments and tool progress are transient.
Treat size, transaction count and WAL growth as acceptance criteria. Never replay
an external effect whose outcome is unknown after interruption.

Context preparation is a small replaceable boundary, with a working ordinary
history default. Keep original history available. The serving view belongs to the
host and is disposable; initially it can be an in-memory SQLite projection.

Preserve existing BubblePaw repositories and data. Keep material architecture
choices visible to the user, but do not turn routine work into repeated questions.
Use native workers for substantial scoped work; maintain clear file ownership.
Run formatting, strict Clippy and focused behavior tests. Distinguish scripted,
local transport and live model acceptance. Do not add AI Git attribution.

The project wiki is `docs/`, with its index at `docs/README.md`. Read the index first and keep pages current; add new pages to the index.
