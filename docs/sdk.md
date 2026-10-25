# Rust SDK

Status: built 2026-10-25 for 0.2.0.

`bubblepaw-sdk` is the supported Rust surface for embedding BubblePaw. It is the
one dependency an outside program needs. It re-exports curated types from
`bubblepaw-core` and `bubblepaw-runner` and adds `HostBuilder` for the usual
wiring. It is a facade inside this workspace: it releases with every BubblePaw
tag, so embedders do not depend on internal crate paths.

## Depend on it

```toml
bubblepaw-sdk = { git = "https://github.com/BubbleBuffer/BubblePaw-Core", tag = "v0.2.0" }
```

`v0.2.0` is the first tag with the SDK. The crates are not published to
crates.io, and the SDK is not a protocol client.

## Modules

- `store`: the authoritative archive: `Store`, `Session`, `Message`, roles,
  chapters and queued input.
- `harness`: `Harness`, `HarnessConfig`, `RunOptions`, `ExecutionMode`,
  `CancellationToken` and the other run types.
- `events`: `HarnessEvent` and `EventKind`.
- `approvals`: approval policy, modes, declared approval and the decision record.
- `tools`: the tool traits and registry, plus the runner's workspace, process,
  history, JavaScript and artifact capabilities.
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

## HostBuilder

`HostBuilder` replaces the wiring an embedder writes by hand. `build` must run
inside a Tokio runtime context, because the harness spawns its runs there.

```rust,ignore
let host = HostBuilder::new(store, providers)   // Store, ProviderRegistry
    .workspace(workspace_dir)                   // binds and creates the workspace
    .tools(registry)                            // your own ToolRegistry (optional)
    .history_tools(true)                        // history_search, history_read, history_chapters
    .code_mode(artifacts_dir)                   // ArtifactStore + CodeMode
    .wrap_support_tools(|tool| tool)            // optional host-tool decorator
    .summarize(SummaryConfig::default())        // readable summarization
    .context(|summary| Arc::new(summary))       // optional own ContextPreparation
    .config(harness_config)                     // your HarnessConfig
    .build()?;                                  // -> Host
```

Only `workspace` is required: it binds the store to the
workspace and creates the directory when missing. `history_tools` and
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

`crates/bubblepaw-sdk/examples/scripted_chat.rs` is the runnable version: an
in-memory store, a scripted provider that calls one host tool, a harness run and
the final text. It needs no network.

```sh
cargo run -p bubblepaw-sdk --example scripted_chat
```

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
