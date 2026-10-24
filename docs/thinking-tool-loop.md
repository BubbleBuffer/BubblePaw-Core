# Thinking replay in tool loops

Live provider configuration also binds replay to the configured route and
endpoint. States without `_bubblepaw_route_origin` inherit the current route when
adapter/model are compatible, preserving unfinished signed loops after a plain
upgrade. Only a recorded route/endpoint mismatch is incompatible.

Status: 2026-10-14 live reproduction confirms the child-control prefix bug.
The fixed signed child passes live in two-round and first-round exhaustion cases;
[qualification](provider-qualification.md) records all ten live requests and gates.
Runner inner loops were replaced by hidden background child workers the same day;
that worker change made no live model calls.

## Cause and evidence

Signed thinking binds the top-level system prompt, tool declarations and earlier
messages, as well as its own opaque contents. The current tool-loop guard checks
adapter/model compatibility and missing native state; it cannot detect a changed
conversation prefix. See [Anthropic preserved thinking](https://platform.claude.com/docs/en/build-with-claude/preserved-thinking).

The original budget countdown in `bubblepaw-core/src/runtime.rs` appends a different
remaining count to `ModelRequest.system_prompt` on each round. A signed response
therefore becomes invalid on the very next request. This is the reproduced cause
of the current research-child HTTP 400, rather than loss of a thinking block.

The failing synthetic live capture is under the gitignored
`.run/live400-failure/` directory (original remote run
`e34129c4-fcac-4638-856a-220ec4a8c678`):

- Request 1 states `6 tool calls left`; request 2 states `3 tool calls left` in
  the top-level system prompt. Tools and the earlier user message are identical.
- The raw first SSE has `thinking`, then `tool_use`. Thinking text is empty,
  with a complete 1,760-byte signature. The follow-up carries that exact block
  and signature in the same position. Empty displayed thinking is a real native
  response here, not a parser omission.
- The HTTP 400 body identifies `messages.1.content.0` and says the block is bound
  to a different conversation because the system prompt differs. The shared
  diagnostic helper exposes this type, message and sanitized body to the caller.
- A first, simpler live child passes because adaptive low effort emits no
  thinking blocks. It does not qualify the signed path.

The parser, replay metadata and core loop at the investigation's starting commit
`aded60e` match the consumer's pinned `6068a2c` for these paths. The consumer's
context preparation copies full history; it sets low effort and a 16,384-token
ceiling. Its research child uses a findings/source IDs/gaps/usable schema and a
six-call budget. Consumer inspection remains read-only.

## Cause and limits

The Messages request fallback in `provider/messages.rs:97` (`:77` before the
fix at base commit `a1eb46b`) reconstructs an
assistant turn from canonical text and tool calls when compatible replay state
is unavailable. That reconstruction cannot recover signed `thinking` or
`redacted_thinking`. Current-loop tool turns therefore need compatible state;
the adapter rejects missing or incompatible state locally. Older completed
turns can use plain text and calls. A compatible native response that contains
no reasoning is valid and does not require invented thinking.

This identifies a real silent-loss path, separate from the reproduced cause of
the recorded HTTP 400 above. Read-only consumer inspection shows:

- `crates/pipeline/Cargo.toml` pins BubblePaw revision `7c5567269d45d25e7169154f3c08da864bd0caf6`.
  `Cargo.lock` agrees, including at the evaluated consumer source commit
  `794ecf9` named by the saved Run 3 verification metadata.
- `crates/pipeline/src/main.rs:92` registers `ProviderEndpoints`; its
  `opencode-messages` route uses the Messages adapter and the OpenCode Go
  endpoint (or the configured local metering relay).
- `crates/pipeline/src/lib.rs:880` runs research with a `ParallelJob` and
  `ToolLoop`, rather than BubblePaw's `research` host tool. The pinned manual
  path and the current normal child Harness carry the response's provider state
  into assistant history. The relay forwards the original request bytes.
- `eval/cnn-2024-06-27-0205-1105/RESULTS.md:319` retains only allowlisted error
  diagnostics: `invalid_request_error`, `messages`, `thinking`. It explicitly
  leaves the exact rejection unresolved. No raw error, model transcript,
  credential, or live request is needed for the local regression suite.

Chat defaults to `tool_loop`, including models omitted from configuration.
Explicit `none` and `all` remain available. Generated runtime host notices keep
the current loop open; a new user task or final assistant answer closes it.
Storage preserves all complete
reasoning artifacts; replay still requires the compatible adapter and model.
No store schema changes are involved.

A `thinking` block cut by `max_tokens` before its signature arrives is kept.
`provider/messages.rs` (`parse`, marker `UNSIGNED_CUT`) stores it in the reply's
`provider_state` with its streamed bytes and `"unsigned_cut": true`. Every
Messages request builder leaves marked blocks out, so no continuation, tool
round or later turn sends them. An assistant turn that holds only such blocks
is skipped. The rule for interrupted or cancelled streams is unchanged.
Regression tests: `tests/provider_continuation.rs`
(`messages_continuation_omits_a_thinking_block_cut_before_its_signature`,
`messages_withheld_thinking_stays_out_of_tool_rounds_and_next_turns`).
Chat `reasoning_replay` does not apply to Messages.

The Claude Code extension captures opaque content starts and appends thinking
and signature deltas in `extensions/claude-code/src/provider.ts:574`, stores
them when a step finishes, and hydrates them for a compatible rebuild. Its
tools acceptance test treats an empty artifact set as a replay-only skip while
keeping tool delivery, state persistence and provenance checks. Nonempty
artifacts still require byte-exact replay. The SDK fixture is not a witness of
the underlying live model wire.

## Fix

Core queues the budget as a `tool_budget` reminder, a generated user message in
canonical history. The system prompt stays unchanged. While a limit is set, the
reminder is sent once at run start with the limit and remaining count, and once
more as a warning when remaining calls drop to max(2, 20% of the limit) or fewer.
Each is appended after the thinking it follows; other rounds add no reminder row. Tool results also end with the
remaining-calls footer. There is no store schema change.

Budget exhaustion and schema correction first keep the declarations in
`ModelRequest.tools` and queue a `tools_off` reminder: "Tool calls are unavailable
for the rest of this run. Answer in text using the information you already have."
That round is an ordinary append with the same declarations and the same wire tool
choice, so the signed thinking prefix is untouched. Only if the model still returns
a call does the call get an error result without running, and one enforcement round
follow. Core then moves the declarations into the optional transient
`ModelRequest.disabled_tools` field, declares `tools_off_enforced`, and every adapter
sends the original declarations with the provider's tool choice off:
`tool_choice: {type: "none"}` for Messages, `"none"` for Chat Completions and
Responses. Removing the declarations up front would also invalidate the thinking
prefix. Tool-call parsing and the dispatcher continue to reject unavailable/exhausted calls.

The fix preserves the signed artifact. It does not enable the provider's
`drop_block` beta behavior or rewrite native content. Ordinary history remains
available, and the current-loop missing/incompatible-state guard remains useful.
This repair concerns complete signed responses; the unsigned `max_tokens` case
is covered in the paragraph above.

## Acceptance (2026-10-14)

The dedicated `tests/research_live400.rs` uses an ordinary research child,
OpenCode Messages/Haiku 5.5, low effort, the consumer-shaped schema and output
ceiling, and `max_tool_calls: 6`. A loopback search/fetch fixture makes one actual
search and fetch; repeated cached host calls exercise the six-call boundary.
The default case uses two sequential JS rounds and a final tools-off JSON answer.
An optional case spends the budget in the first JS round.

The offline record-replay path consumes real sanitized SSE through a fake HTTP
transport. It independently reconstructs all original blocks, checks signed
thinking before tool use, exact block/signature bytes, stable system/tools and
unchanged earlier messages. The first fake request establishes its prefix;
follow-ups enforce it, so the test catches the original countdown bug rather
than merely accepting an already-corrupted recorded request.

Final request totals, the pre-fix failing replay, live results and required gates
are recorded in [provider qualification](provider-qualification.md).

## Inner-loop inventory (updated 2026-10-14)

| Location | Behavior | Classification |
|---|---|---|
| `crates/bubblepaw-runner/src/parallel.rs` (`run_jobs`; former `execute` at 281) | Starts independent hidden normal children with per-job profiles and controls. | Former hand-built agent loop removed. Results are acknowledged after caller delivery/persistence. |
| `crates/bubblepaw-runner/src/probes.rs` (`answer`; former round loop at 580) | Uses a hidden child Harness with frozen source context and source-bound history tools. | Former hand-built probe agent loop removed. Replay persists its probe artifacts before acknowledgement. |
| Former `parallel.rs::model_step` | Same-request retry/backoff policy moved to a runner provider decorator used by the child Harness. | Transport-only retries preserved; the decorator never executes tools or builds history. Published errors remain terminal. |
| `crates/bubblepaw-runner/src/research.rs` | Builds the restricted ordinary research profile Harness; the parent starts and continues it through the normal agents tools. | Uses child sessions and the real runtime. The separate `research` tool and its quick/thorough config were removed on 2026-10-13; there is no hand-built research loop. |
| `crates/bubblepaw-runner/src/agents.rs` (`start_profile_owned`, `send`, `observe_turn`) | Starts the profile Harness for new and continuing children and supervises completion/cancellation. | The same real runtime consumer now also launches hidden workers. |
| `crates/bubblepaw-runner/src/codemode.rs:1396` | Pumps JS jobs and host tool calls. | No provider calls or model loop. Left unchanged. |
| `crates/bubblepaw-runner/src/provider.rs:576` | Continues Responses output-limit stops; transport retries live in `post`/`send_round`. | Adapter continuation/retry, no tool execution. Left unchanged. |
| `crates/bubblepaw-runner/src/provider/messages.rs:626` (`generate`, loop at 655) | Continues Messages output-limit stops. | Adapter continuation, no tool execution. Left unchanged. |
| `crates/bubblepaw-runner/src/provider/chat.rs:750` | Continues chat output-limit stops. | Adapter continuation, no tool execution. Left unchanged. |
| `crates/bubblepaw-runner/src/episode_models.rs:161` | Selects an available summary/chapter role model and retries a tool-free request. | Model fallback/retry, no agent loop. Left unchanged. |
| `crates/bubblepaw-runner/src/episode_models.rs` (`summarize`, former repair loop at 513) | Reissues the identical tools-disabled request once when structural validation fails; does not append the previous answer. | Bounded request retry, no multi-step agent/tool history. Left unchanged. |
| `crates/bubblepaw-runner/src/model_roles.rs:200` | Repeats and aggregates decision samples, with bounded safe retries. | Decision sampling/retry, no tool execution. Left unchanged. |
| `crates/bubblepaw-runner/src/summarization.rs` and `episode_worker.rs` / `episode_background.rs` | Prepare context and schedule derived summary/episode work through `EpisodeModels`; they do not dispatch model tools. | Orchestration of direct tools-disabled summary calls. No inner agent loop. |
| `crates/bubblepaw-runner/src/probes.rs::judge` | One tools-disabled grading request with strict judgment parsing. | Direct one-shot retained; adding a child would add no tool/runtime behavior. |
| Chat titles in `agents.rs`, `web.rs`, `main.rs`, imports and episode parsing | Literal caller/task text or a title extracted from the existing summary response. | No separate title model call exists; no title worker is needed. |
| `extensions/claude-code/src/provider.ts:459`, `:717` | Consumes the SDK query and parks host tool calls until the next harness step delivers results; the one-off path consumes a tool-free query. | Stateful provider adapter, no host-side agent/tool execution loop. Left unchanged. |

The authoritative model/tool loop is `crates/bubblepaw-core/src/runtime.rs`.
The recorded-corpus replay loop in `runner/src/replay.rs` imports recorded
messages and evaluates context; it does not execute a live research agent loop.

The checks for background workers are listed in [verification](verification.md). Probe exchanges preserve
generated origins, tool-error flags, child session identity and the history cursor.
Both configured probe models use the normal registry adapter and reasoning replay;
the scripted/raw fixture path removes host-only metadata before HTTP and metering.

## Historical local verification (2026-10-13)

The pre-fix regression runs on Linux with the original adapter/default source
from base commit `a1eb46b` and the new real-runtime fixtures:

- Messages and Responses preserve complete native state across all three tool
  rounds even before the fix.
- Erasing Messages state only from the serving request makes the original
  fallback send a thinking-less follow-up; the fake endpoint rejects the opaque
  byte check.
- Chat's original `none` default omits `reasoning_content` on the follow-up;
  its fake endpoint rejects the byte check.

That run has two expected failures and two passes (exit 101). It isolates the
reproducible gaps without attributing the historical consumer rejection to
either one. The fixed real-runtime fixture passes all four cases. On every
follow-up, context preparation reloads SQLite rows (including blob-backed
provider state), the fake HTTP endpoint compares the exact native opaque
blocks/fields and their position, and each tool confirms its assistant state
is durable before dispatch. The missing-state case also checks a nonretryable
local `Fatal` and an untouched durable original.

Additional Linux checks pass: nine Messages boundary tests, eight replay
policy tests, two policy/config tests, 22 web provider tests with the `web`
feature, 45 provider settings frontend tests and its TypeScript check, and nine
Claude Code opaque replay fixtures plus its TypeScript check. The extension
uses Bun 1.3.14 and the frontend uses Node 22.18.0 from temporary test binaries;
their temporary directories are removed at exit. Every model transport is a
local synthetic fixture. All 48 live tests remain ignored.

The required Linux gates run with Rust 1.97.1:

- `cargo +1.97.1 fmt --all --check` passes, including after the final fixture
  correction.
- `cargo +1.97.1 clippy --workspace --all-targets -- -D warnings` passes.
  The same workspace check with `--features web` also passes. A final focused
  Clippy check of `provider_host_notices` passes after its fixture correction.
- `cargo +1.97.1 test --workspace --no-fail-fast` runs once and exits 101:
  831 tests pass, two fail, and 59 are ignored. All new reasoning loop fixtures
  and all 33 reasoning-storage tests pass. `provider_host_notices` initially
  supplies a current assistant tool turn without native state; its corrected
  fixture supplies signed thinking and checks the entire assistant content
  byte-exactly. All three tests in that target pass on the focused rerun.
- The other full-run failure is
  `native_effects::pending_completed_run_recovers_unknown_without_replaying_process`
  at `crates/bubblepaw-runner/tests/native_effects.rs:179`: immediate store
  reopen fails with an ownership lock error. All three tests in that target
  pass on the focused rerun without changes to that test or its production
  code. Source inspection suggests a pre-existing teardown race: `CodeMode`
  Drop at `codemode.rs:897` sends Close without joining the JS thread, while
  the last Store owner can still run `Inner::drop` at
  `crates/bubblepaw-core/src/store_serving.rs:529` and retain its lock after
  the weak owner is no longer upgradeable (`store_layout.rs:590`). This is an
  inference, not a repaired or conclusively reproduced race. The full
  workspace gate remains failed; the passing rerun does not replace it.

The 48 paid provider tests are ignored; the private transcript fixture is
unavailable because `BUBBLEPAW_FIXTURES_DIR` is unset. No store schema changes,
consumer edits, live calls, merge or push are involved.

## Provider reload replay boundary (2026-10-14)

Live configuration apply preserves stored thinking and reasoning. Replay origin
checks compare any recorded provider route/endpoint alongside adapter/model
identity. Recorded artifacts from a different endpoint cannot be forwarded to
its replacement; absent route tags inherit the current compatible route. A request already in flight
keeps its old snapshot; the next model request or tool round uses the applied
configuration. These guards do not establish live signed-thinking acceptance for
arbitrary provider switches; the recorded/live evidence above retains its scope.
