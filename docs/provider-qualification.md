# Provider qualification

Status: reference, current as of 2026-10-24.

A provider kind is qualified when it meets the bar below at each level that
applies. The levels are reported separately, because a passing local test says
nothing about a live provider, and the reverse is also true. For how the tests are
run, see [verification](verification.md).

[Providers](providers.md) refers to this bar by its numbered items.

## Acceptance levels

- **S, scripted:** in-process, no transport. Core tests, and the fake Agent SDK for
  the Claude Code extension.
- **L, local transport:** a loopback HTTP server or child process replays synthetic
  wire streams, using the `serve` and `raw_server` helpers in
  `crates/bubblepaw-runner/tests/provider_*.rs`. The recorded research-child stream
  in `crates/bubblepaw-runner/tests/fixtures/research_live400/` is synthetic task
  content that a live provider returned, replayed through a fake transport.
- **V, live model:** env-gated ignored tests in `crates/bubblepaw-runner/tests/provider_live.rs`.
  Gates are listed in [verification](verification.md). Live runs make
  paid calls and are not part of CI.

## The bar

A provider kind is qualified when each applicable item has an L test (S for the
extension's SDK side), and a V test exists that skips cleanly without keys.

1. **Streaming text:** SSE is assembled into one complete message. Listener text is batched.
2. **Tool calls:** one and several calls in one turn, with normalised ids and a
   result round trip. Malformed calls are metered and never executed.
3. **Cancellation:** an open stream and a retry backoff can be interrupted.
4. **Resume after interruption:** pre-cancelled or unknown-outcome calls never reach
   the transport. Unknown effects are never replayed. The core guarantees this, and
   each adapter has one test.
5. **Continuation:** a length stop is continued and joined. A cut tool call is
   dropped. A limit returns an incomplete result. An output limit is sent only when
   the role sets one.
6. **Reasoning artifacts kept:** each round's artifacts are stored once, byte-exact.
7. **Reasoning replay:** replayed only to the origin provider and model. Foreign or
   mismatched opaque state is omitted.
8. **Error mapping:** 401 and 403 map to Auth. 429 maps to RateLimit, with
   `resets_at` and Retry-After. Context overflow maps to Context. 5xx and connection
   failures map to Transient. Mid-stream error events are mapped. A failure after
   text has been published is not retried (`ProviderError.published`). Diagnostics
   are bounded and sanitised, and credential echoes are redacted.
9. **Token usage:** input, output, cache read, cache write and, where reported,
   reasoning tokens map into `Usage`.
10. **Structured output:** the native schema wire shape, a typed round trip, and an
    invalid schema that fails before any HTTP request.
11. **Effort:** the role or config effort reaches the native field. Reasoning stays
    on, with low as the default.
12. **Auth:** the header shape is asserted. File-based tokens are re-read. Missing or
    malformed credentials have error paths.

Image input is a separate capability ([providers](providers.md)) and is not part of
this bar.

## Status by kind

Scripted and local-transport results are from the automated suite. Live results
are from the env-gated tests.

| Kind | S (scripted) | L (local transport) | V (live) |
|---|---|---|---|
| codex and responses | Core resume (item 4). | All items 1 to 12. Bearer auth, `auth.json` errors and a 401 re-read are covered. | Responses on the OpenCode Go route: stream, tools, cancel, continuation, schema and bad_key pass. On OpenRouter every scenario passed in a follow-up run. Codex has an earlier live tool loop and restart-continuation proof with `gpt-6-luna`. The codex scenario suite has not been run. Direct OpenAI has no live run. |
| anthropic-messages | Core resume (item 4). | All items 1 to 12. `x-api-key` auth. | Messages on the OpenCode Go route with `claude-haiku-5-5`: all six scenarios pass. The tools scenario skips its reasoning-replay check, because low effort returned no replayable artifact. Direct Anthropic has no live run. |
| chat-completions | Core resume (item 4). | All items 1 to 12, including function calling. Bearer auth. | Chat on the OpenCode Go route with `deepseek-v4.1-flash`: five scenarios pass. The schema scenario is flaky (see below). On OpenRouter with `openai/gpt-oss-20b`, every scenario passed in a follow-up run. Direct chat endpoints and `deepseek_chat` have no live run. |
| Claude Code extension | Items 2, 7, 8, 9, 10 and 11 (S or S/L). | Items 1, 3, 4 and 6 at L, and items 2, 8 and 10 at S/L. Item 4: a killed runner leaves one unknown effect, which is never reissued. | Earlier live run: stream, tools, cancel and schema, three of four passing. The tools scenario was skipped because the model returned no thinking. Not re-run since. |

Items marked n/a for the extension: continuation (item 5), because the SDK owns
its turn, and auth (item 12), because the SDK owns login. Auth errors are still
mapped.

## Known gaps

- The 48-scenario live suite is only partly run.
- Direct OpenAI chat, direct Anthropic, direct DeepSeek chat, and the codex
  scenario suite have no live run.
- The extension has not been re-run live since its earlier run.
- The OpenCode Go chat schema scenario is flaky. Some attempts fail with an upstream
  HTTP 400 before the adapter parses any response.
- Small models were tried on OpenRouter for the full feature set. Every one showed
  failures, so none is a qualified default.

## Rules that qualify the bar

- **Codex OAuth.** BubblePaw never refreshes or writes `auth.json`. It re-reads the
  file on every attempt, and again on a 401. The Codex CLI owns refresh. A second
  writer for rotating refresh tokens would log the user out.
- **Signed research-child replay.** `research_live400.rs` checks the thinking,
  signature, text and tool-use block order and bytes offline, against the committed
  fixture. The offline test needs no credentials:
  `cargo test -p bubblepaw-runner --test research_live400`. Its live case is gated
  by `BP_LIVE_RESEARCH_400=1`. No recorded signature is sent to a live service.
- **Disabled tool declarations** are request metadata for one turn. They are not
  stored in provider state or SQLite.
