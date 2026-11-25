# Layering

The planned split separates model access, in-memory runs and durable orchestration.
The layering guard keeps the boundaries measurable while the split is in progress.

- `bubblepaw-ai` talks to models and knows no sessions or stores.
- `bubblepaw-agent` runs one run in memory and knows no database, HTTP or feature.
- `bubblepaw-core` makes runs durable and knows no model API, tool or feature word.

Normal and build dependencies obey these rules; dev-dependencies are exempt:

- AI and process depend on no workspace crate.
- Agent depends only on AI among workspace crates, and not on `rusqlite` or `reqwest`.
- Core depends only on AI and agent among workspace crates, and not on `reqwest`.
- No workspace crate depends on SDK; only SDK depends on runner.

The guard skips rules for crates whose folders do not exist yet. Existing forbidden
dependencies are listed in `layering-allowlist.txt` and must be removed from the
list when the dependency disappears. New violations fail.

The guard counts case-insensitive substrings in every Rust source file, including
comments. Core and agent track `episode`, `chapter`, `child`, `opencode`, `quota`
and `"js"` (including the quotes). AI tracks `session`, `rusqlite`, `approval`,
`episode`, `chapter` and `child`. Missing entries have a ceiling of zero.
An increase fails and reports the ten files with the most hits. When a count falls,
lower its ceiling in `layering-allowlist.txt` to the new total; never raise it.
