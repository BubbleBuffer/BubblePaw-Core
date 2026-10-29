# Rust SDK

Status: built through 2026-10-27, including additive context-fact reads and host user answers to child questions.

`bubblepaw-sdk` is the supported Rust surface for embedding BubblePaw. It is the
one dependency an outside program needs. It re-exports curated types from
`bubblepaw-core` and `bubblepaw-runner` and adds `HostBuilder` for the usual
wiring. It is a facade inside this workspace: it releases with every BubblePaw
tag, so embedders do not depend on internal crate paths.

## Depend on it

```toml
bubblepaw-sdk = { git = "https://github.com/BubbleBuffer/BubblePaw-Core", tag = "v0.3.0" }
```

`v0.2.0` is the first tag with the SDK; `v0.2.1` adds the todo module, redacted fatal error text and the fork and spool-sweep race fixes; `v0.3.0` adds the cmd shell and scratchpads for exec on Windows and the opt-in chapter prompt v2. The crates are not published to
crates.io, and the SDK is not a protocol client.

Fixes reach BubblePaw-Core only through a sync: once they are merged into the
development repo's `release` branch, `scripts/publish-public.sh --push` (run from
`release`) commits the exported tree to BubblePaw-Core. Consumers that pin a Core
revision or tag, such as Scratch, then bump that pin. The 2026-10-25 todo SDK
module and the redacted fatal error text reach consumers this way.

## Modules

- `store`: the authoritative archive: `Store`, `Session`, `Message`, roles,
  chapters and queued input.
- `harness`: `Harness`, `HarnessConfig`, `RunOptions`, `ExecutionMode`,
  `CancellationToken` and the other run types.
- `events`: `HarnessEvent` and `EventKind`.
- `approvals`: approval policy, modes, declared approval and the decision record.
- `tools`: the tool traits and registry, plus the runner's workspace, process,
  history, JavaScript and artifact capabilities.
- `todo`: the `todo` tool, its `TodoItem` / `TodoStatus` host state, `list`
  and the `HOST_STATE_KEY` it is stored and announced under.
- `questions`: `QuestionTools`, `WakeCoordinator`, `ChildQuestionAnswerError`
  and the typed `PendingChildQuestion` host view.
- `providers`: the provider request surface, the decision types used by roles,
  and the runner's HTTP, extension and scripted adapters.
- `context`: the context preparation hook, `ContextWriter`, and the runner's
  summarizer.
- `net`: web search and fetch settings. Always available; the `net` module of
  the runner is not behind the `web` feature.
- `web` (feature `web`): the local web host configuration.
- `claude_code`: the bundled Claude Code extension folder and its
  `ExtensionConfig`.
- `prelude`: the dozen items nearly every embedder needs.

`bubblepaw_sdk::core` and `bubblepaw_sdk::runner` re-export the whole crates.
They are an escape hatch with no stability promise.

## Context facts

`bubblepaw_sdk::store` exposes `ContextEvent`, `ContextEventKind`,
`ContextTrigger`, `ContextView`, `ContextBlock`, `ContextBlockKind`,
`ContextEpisode`, `ContextSubjectChapter`, `SubjectChapterStatus`,
`MessageTokens` and `MessageTokenSource`. No core escape hatch is needed for
these reads on its re-exported `Store` and the combined SDK token read:

```rust,ignore
let episodes = store.context_episodes(session_id)?;
let chapters = store.context_subject_chapters(session_id)?;
let current_view = store.context_view(session_id)?;
let events = store.context_events(session_id)?;
let tokens = bubblepaw_sdk::store::message_tokens(&store, session_id, first_seq..=last_seq)?;
```

`context_events` returns stable chronological insertion order. Message ranges
use inclusive canonical sequence numbers; `message_seq(session_id, message_id)`
resolves an ID. Token rows distinguish `Reported` from `Estimated`. A view's
`blocks` and `blocks_estimated` describe its apply-time token partition; the
block list can be empty for a legacy view. See [context compaction](context-compaction.md)
for triggers, coverage and estimate semantics. The SDK-only integration test is
`crates/bubblepaw-sdk/tests/context_facts.rs`.

Core's `Store::reported_message_tokens` returns optional reported counts without
decoding message bodies. The SDK fills missing counts with the runner's existing
estimator; no estimator or tokenizer dependency is added to core. For inherited
compaction coverage without stored source counts, an event's `tokens_before` is
null while its covered range remains available.

The same module exports typed host reads: `context_view`, `context_structure`,
`context_summary`, `context_events`, and `history_range`, with `ContextStructure`,
`ContextSummary`, `SummaryKind`, `HistoryRange`, `HistoryRangePage`, and metadata
DTOs. These share the implementation behind the stdio `context` feature and web
GET routes documented in [Session API](session-api.md); all reads write nothing.

## HostBuilder

`HostBuilder` replaces the wiring an embedder writes by hand. `build` must run
inside a Tokio runtime context, because the harness spawns its runs there.

```rust,ignore
let host = HostBuilder::new(store, providers)   // Store, ProviderRegistry
    .workspace(workspace_dir)                   // binds and creates the workspace
    .tools(registry)                            // your own ToolRegistry (optional)
    .history_tools(true)                        // history_search, history_read, history_chapters
    .todo(true)                                 // the todo tool; read with host.todo(session)
    .questions(questions)                       // shared Arc<QuestionTools> (optional)
    .code_mode(artifacts_dir)                   // ArtifactStore + CodeMode
    .wrap_support_tools(|tool| tool)            // optional host-tool decorator
    .summarize(SummaryConfig::default())        // readable summarization
    .context(|summary| Arc::new(summary))       // optional own ContextPreparation
    .config(harness_config)                     // your HarnessConfig
    .build()?;                                  // -> Host
```

Only `workspace` is required: it binds the store to the
workspace and creates the directory when missing. `history_tools`, `todo`, `questions` and
`code_mode` register host capabilities; `wrap_support_tools` decorates every one
of them before it enters the registry, and their shown-always status survives.
`summarize` builds the runner's `SummarizingContext` over the store and the
provider registry, with the runtime notes when code mode is on. `context` wraps
that summarizing context in your own `ContextPreparation`; it needs
`summarize`.

The builder fills `HarnessConfig::context` and leaves the rest of the
configuration alone. `build` fails with a clear error when the final registry
exposes no model tool for the configured execution mode.

`Host` holds `harness: Arc<Harness>`, `store`, `providers:
Arc<ProviderRegistry>`, `code_mode: Option<Arc<CodeMode>>` and `tools`, a
snapshot of the final registry for hosts that also serve tools outside a run.

## Todo list

`HostBuilder::todo(true)` registers the `todo` host tool (2026-10-25). The
model replaces the whole list with `{"items": [{"text", "status"}]}`, where
status is `pending`, `in_progress` or `done`, at most 50 items. The latest list
per chat is host state under the key `todo`, never part of the prompt.
`Host::todo(session_id)` (or `todo::list(&store, session_id)`) reads it, empty
when none was set. Each change emits `EventKind::HostStateChanged { key:
"todo", value }` with the bare item array. In native mode the tool is a model
tool (also through the Claude Code extension, which serves it over MCP); in
JavaScript mode it is `tools.todo(...)`.

`crates/bubblepaw-sdk/examples/scripted_chat.rs` is the runnable version: an
in-memory store, a scripted provider that calls one host tool, a harness run and
the final text. It needs no network.

```sh
cargo run -p bubblepaw-sdk --example scripted_chat
```

## Child questions

`Host::pending_child_question(child)` returns
`anyhow::Result<Option<questions::PendingChildQuestion>>`. The view contains
`id`, `text`, `options: Vec<String>`, `asked_at: Option<String>` (RFC 3339), and
`parent_session_id`. Older saved questions have no timestamp. Missing, archived
and root sessions, or a child without a pending question, return `None`. This
reads durable state even when the local question service is disabled.

To answer, inject the question service used by your child harnesses with
`HostBuilder::questions(Arc<QuestionTools>)`. Construct it through stable SDK
paths, using the builder's store:

```rust,ignore
use bubblepaw_sdk::questions::{QuestionTools, WakeCoordinator};
let questions = Arc::new(QuestionTools::new(
    store.clone(), Arc::new(WakeCoordinator::default()), true,
));
let host = HostBuilder::new(store, providers)
    .workspace(workspace)
    .questions(questions)
    .build()?;
if let Some(question) = host.pending_child_question(child)? {
    host.answer_child_question(&question.parent_session_id, child, &question.id, "Green").await?;
}
```

`Host::answer_child_question(parent, child, question_id, text).await` returns
`Result<(), questions::ChildQuestionAnswerError>`. `NotFound` covers missing or
inactive children, wrong direct parents and unknown question IDs; `Conflict`
covers already answered or cancelled questions; `Invalid` covers blank answers
and answers over 16 KiB; `Internal` carries a host/delivery error. Without an
injected service, answering returns `Internal`.

The builder registers the ordinary `question` tool and binds its own harness as
the fallback for children run directly by the embedder. A service also bound to
an agent manager retains managed children's original harness and profile. The
user-answer operation is a host API and adds no model tool. A live child tool
receives the answer; an interrupted waiter gets a safe generated continuation.
The child's question state records `answered_by: "user"`. Durable
`host_state_changed` events update the child's `question` and the parent's
separate `child_question_answer` key, a bounded latest-answer projection with
`child_session_id`. The parent's own pending question is unchanged. See
[child sessions](child-sessions.md) for delivery and recovery semantics.

## Claude Code extension

`claude_code::bundled_dir()` is the `extensions/claude-code` folder of the
BubblePaw source the crate was built from. Cargo keeps the whole checkout for a
path or git dependency, so this works for both.
`claude_code::extension_config(id, bun, dir)` builds the matching
`ExtensionConfig`: `bun run <dir>/src/main.ts` with `dir` as its working
directory.

## Stability

Everything reachable through the named modules, `Host` and `HostBuilder`
follows semantic versioning from 0.2.0. A breaking change there is a version
bump.

`bubblepaw_sdk::core` and `bubblepaw_sdk::runner` are an escape hatch with no
stability promise. Anything you reach through them may change in a patch
release.

## Other consumers

Non-Rust programs do not need a client library. They use the stdio JSON-RPC
interface described in [session-api](session-api.md), or the optional HTTP host
described in [web-contract](web-contract.md).
