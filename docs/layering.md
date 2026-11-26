# Layering

The split separates model access, in-memory runs and durable orchestration.
`bubblepaw-ai` exists; `bubblepaw-agent` does not yet. The layering guard keeps the
boundaries measurable while the split is in progress.

- `bubblepaw-ai` talks to models and knows no sessions or stores.
- `bubblepaw-agent` runs one run in memory and knows no database, HTTP or feature.
- `bubblepaw-core` makes runs durable and knows no model API, tool or feature word.

## The ai crate

`bubblepaw-ai` holds what passes between a harness and a model: the message,
content block, tool call and tool definition types, `ModelRequest`, `ModelResponse`,
`Usage`, the stream events, the cancellation token, the provider error and capability
types, the history cursor, wire shapes for cache accounting, and the `Provider`
trait. It depends on no workspace crate, and its dependency list is external crates
only. `bubblepaw-core` depends on it and re-exports every moved item at its old
path, so embedders import from `bubblepaw_core` as before.

The provider adapters (chat completions, Anthropic messages and responses), their
HTTP client, stream, retry and error handling, the request dump and the scripted
model live in `bubblepaw-ai/src/adapter`, behind the `http` feature. Only the
runner's `host` feature enables it, so a client-only build links no HTTP client. The
adapters reach the host through injected seams: a `ModelEgress` trait for proxied
egress, a `UsageSink` that the core capture scope installs, and a host-policy flag for
quota stops. `bubblepaw-runner` re-exports every moved name at its old path.

The extension provider, which speaks line-delimited JSON-RPC to a subprocess,
lives in `bubblepaw-ai/src/extension`, behind the `extension` feature; the runner's
`host` feature enables it. The provider builds the command and runs the protocol,
and a launcher the host passes to `ExtensionProvider::new` starts it. The launcher
returns the pipes and a `LaunchedProcess` that kills the whole process tree on
demand and on drop; the runner's launcher uses `bubblepaw-process`, so ai depends
on no workspace crate. The runner builds the provider with `extension_provider`,
reads the extensions file, and re-exports the moved names at their old paths. The
Claude Agent SDK rate-limit events the extension reports are parsed in
`adapter/quota_events.rs`.

The model catalog is `bubblepaw-ai/src/catalog.rs` with its data in
`catalog/models.json`, keyed by model name. It holds the dated list prices (the
`PriceTable`, `Rates` and `TokenClasses` types, re-exported by the runner's `pricing`
module), the largest synchronous output per model, the Anthropic cache lifetime, the
smallest cacheable prefix and the cache block size, and a context-window field that no
model fills yet. Each fact keeps its old matching rule. To update a price, edit
`models.json` and its `checked` date.

Still to move: the provider registry, which lives in `bubblepaw-runner` today. Some ai types carry a `session_id` that the ai layer
treats as an opaque key; the adapters add the `x-opencode-session` and Codex
cache-key plumbing, the replay-state and continuation code that handle that key,
and the extension provider keeps a cursor reset per `session_id` and sends
`session.close`; the guard records all of these occurrences as ceilings rather than
renaming them.

## The journal seam

The run loop in `bubblepaw-core` writes things down through the `Journal` trait before it goes on: admission, queued input, replies, tool starts and finishes, outputs and run endings are each one journal call, and the call commits its rows and durable events together. The loop reads pending input and duplicate call ids through the same trait. One implementation, `SqliteJournal`, sits over the store and the event log. Writes that still ride inside a journal commit, such as the execution mode, tool surface and reminders bound at admission, stay in that implementation until they get their own seams.

The dispatcher in `bubblepaw-core` asks a `ToolGate` trait about each call instead of reaching into the approval code. `admits_without_write` says whether a call needs nothing written or asked, so the reply and dispatch still commit together; `before` returns allow, deny with the text the model reads, or cancelled, and may wait for a person; `after` shapes the finished result through the host's result hook. The call travels as plain data. One implementation, the stock gate in `approvals`, holds the policy, session rules, automatic review and workspace roots. The size-limit summary and the nested-call skip stay in the dispatcher.

## Rules

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
