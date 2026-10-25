# Provider extensions

**Status:** approved 2026-10-08; step 1.2 built (below). Keep changes to `runtime.rs`, `store.rs` and the summarizer small; prefer new modules.

**Built (step 1.2, 2026-10-08):** core types, central cursor, per-step tool surface, `ProviderRegistry`, the JSON-RPC extension host (`--extensions <file>`) with a fake extension fixture, and the Responses provider on capabilities and typed errors. `Harness::delete_session` calls `close_session`.

Details pinned while building:

- Cursor: `rewritten_from` is the id in the *new* list at the first differing position; a pure truncation (new list is a strict prefix of the old) gives `None`. The memory advances when the step is dispatched, whether or not it succeeds.
- A fresh cursor handed out by a host (provider switch in the registry, extension restart) keeps its substituted epoch for later appends of the same harness epoch, so the provider rebuilds once.
- Tool surface (user decision 2026-10-08): `Code` has no `also`. The model sees only `js`; Code falls back to Direct when `js` is not among the request's tools. Registries mark always-shown host tools (`ToolRegistry::show_always`; runner default: read, write, edit, search, list, exec, poll, wait, cancel, agent). The `js` description is generated per request from the registry's visible capabilities: always-shown tools with hand-written lines or a schema signature `tools.name({a, b?}) -> JSON: description`, the rest as one sorted name list. `tools.$search(query, {limit})` returns `{tools}` (keyword ranking, name weighted over description, default limit 5, max 50) or, for `select:a,b`, `{tools, unknown}`; `tools.$list()` returns `[{name, summary, shownAlways}]`. Both see only the registry's visible host tools; `wait` stays a global and is not in the deferred name list.
- Summary requests from the summarizer carry no `session_id` (a one-off step outside the session's mirror).
- Extension `capabilities` and `models` are defaults/empty until the extension has started. Start it before introspection when negotiated metadata is needed. The harness selects declarations; the extension adapter forwards that list unchanged so explicit native/hybrid mode and the embedder definition API agree with the wire (2026-10-20).
- `initialize` params are `{"host":"bubblepaw","version"}`. The registry uses the configured `id`; the `provider` field of the result is informational. A relative `cwd` resolves against the extensions file.
- When the harness drops a step on cancellation, the host sends `cancel` and the run fails with the harness's own cancellation message; a direct caller waits up to 5 s for the extension's typed error.
- Exits fail in-flight steps as `transient`; a spawn failure is `fatal`. Each extension stderr line is logged at `warn` (target `bubblepaw::extension`, 2026-10-25) as `extension stderr: <line>`, redacted and cut to 300 bytes like fatal messages (below). Stderr never enters a typed error.
- Responses: 401/403 auth; 429 rate limit with `resets_at` = now + `Retry-After`; 400 with `context_length_exceeded` (or a `response.failed` event saying so) context; 408/409/5xx, transport errors and broken streams transient; cancellation transient `cancelled`; unreadable Codex auth file auth; anything else fatal. Default provider ids: `codex`, `api`, `demo` (the `--provider` value).

**Built (step 1.3, spike, 2026-10-08):** the Claude Code extension in [`extensions/claude-code/`](../extensions/claude-code/) (Bun + TypeScript, `bun test`), a port of pi-claude-bridge's provider mechanism (MIT notice in its `NOTICE`). Run it with an extensions file `[{"id":"claude-code","command":"bun","args":["run","src/main.ts"],"cwd":"<repo>/extensions/claude-code"}]` and `--model claude-code/haiku` (or `sonnet`, `opus`, `claude-opus-5-5[1m]`, ...).

- Models: the aliases `opus`, `sonnet`, `haiku` at 200K, and the `[1m]` ids pcb measured at 1M; any other id is passed to Claude Code as is. All report `cache: session`, `tool_surface: code`, `stateful: true`.
- Paths per step: `deliver` (same epoch, the tool results for the parked calls appended: released into the running query), `reuse` (same epoch, a new prompt after a finished reply: resume the Claude Code session), `rebuild` (anything else: the session JSONL is rewritten from the request with cc-session-io, whole history; a parked query is killed and the session id rotated), `fresh` (no history before the prompt). Our replies carry `provider_state.claude_code = {session_id, tool_use_ids, message_id}`; the next step checks it before reusing.
- `usage.input_tokens` is the whole prompt (uncached + cache reads + cache writes), like the Responses provider; `cache_read_tokens` the cache reads. `BUBBLEPAW_CC_LOG=<file>` appends one JSON line per step (path, reason, usage with cache writes). `BUBBLEPAW_CC_CWD` sets Claude Code's working directory, which also decides where its session files go.
- Session-less steps (summaries) run as one-off queries: custom system prompt, one turn, no tools, `persistSession: false`.
- Ephemeral messages (ids starting `ephemeral:`, see Interface) are not mirror history and are not counted in the mirror's cursor. On `deliver` they go as `<system-note>` text after the last tool result's content; on `reuse` before the new prompt's text. They then stay in Claude Code's own session history, which is accepted. A rebuild writes the request as it is, so only the current ones appear, at their position; a trailing turn of only ephemeral messages is followed by the continue prompt.
- Steering: when the new messages after parked calls are their tool results followed by user messages (the harness's steering boundary), the step still takes `deliver`. The user messages are pushed into the query's prompt stream with priority `next`, and each push waits until it is written to Claude Code's stdin before the tool results are released over the same stdin, so Claude Code drains them at this tool boundary: its turn holds the tool results, then the user text, as the store does. The reply's `provider_state.claude_code.steered` lists the steered message ids; the step log shows `reason: "steered N"`. This ordering relies on Claude Code CLI internals (pcb's note in `prompt-stream.ts`).
- `max_output_tokens` is not supported: the Agent SDK (0.3.293) has no option for it, so it is ignored.
- Measured (haiku, 2 `js` cells, `--no-auto-summarize`): step 1 fresh 4,859 in / 3,847 cache read; steps 2-3 deliver 5,081/4,857 and 5,303/5,079. A follow-up prompt in the same process took `reuse`: 5,936/5,852. A new process (no cursor memory, no mirror) rebuilds: 5,356/3,847, so a rebuild keeps only the system-prompt-and-tools prefix (about 3.8K) and rewrites the conversation part.
- With auto-summarization on (the default) the runtime note used to start a new epoch on every step, so every step rebuilt (steps 2-3 at 3,847 cache read). Since the note is ephemeral and the summary blocks have stable ids, the same task measured (2026-10-08): step 1 fresh 4,921 in / 1,697 cache read; steps 2-3 deliver 5,267/4,919 and 5,552/5,265.
- Steering measured (haiku, a `js` cell running `ping -n 7` through `tools.exec`, `run.steer` one second in): step 2 took `deliver` with `steered 1` in the same query (6,009 in / 4,927 cache read), and the answer followed the steered message. Claude Code's session file holds the tool result (with the runtime note appended) and then the steer as a `queued_command` attachment.

Why: BubblePaw is being embedded in notes-app (spec: `notes-app/wiki/integrations/agent.md`). It must drive very different models through one loop: Codex over Responses, the Anthropic API, DeepSeek, and Claude Code on a subscription, where Claude Code is driven step by step like a model (the pi-claude-bridge approach). Providers become extensions behind one interface, in process or out of process.

## Image input (2026-10-15)

`tools.read({path})` recognizes PNG, JPEG, GIF and WebP by their file signature.
It saves original bytes as a canonical SQLite attachment and returns small image
metadata to JavaScript. A `js` cell automatically includes images from successful
reads in its tool result, including when the script returns another value or
omits `return`. One call attaches at most four distinct images; duplicates attach
once. The per-image limit is exactly 5,000,000 bytes (5 MB). Oversized images and
the count cap return explicit omission notes. There is no resizing or conversion.
If a background cell reads an image after its initial receipt, inspecting that
environment supplies the completed cell's image references to the inspecting call.
These limits concern input; `image()` continues to publish user-facing artifacts.

The provider resolves image references from the canonical store immediately
before sending its request. The native wire shapes are:

| Provider kind | Image placement |
| --- | --- |
| `anthropic-messages` | `image` blocks with a base64 `source` inside the matching `tool_result.content` array |
| `responses` and `codex` | `input_image` parts with data URLs inside `function_call_output.output`, beside `input_text` |
| `chat-completions` | A user message with `image_url` parts after the complete group of parallel tool results; tool messages retain text content |
| Claude Code and other extensions without image capability | A short image-unavailable note; image bytes are omitted |

Responses image parts follow the documented
[OpenRouter tool-output content format](https://openrouter.ai/docs/api_reference/responses/tool-calling).
Repeated image references across history send bytes only at their first visible
occurrence in each prepared request. Later occurrences carry an already-provided
note. If context preparation removes that first occurrence, the first remaining
reference supplies the bytes again.

The generic `Capabilities.image_input` flag defaults to false. Built-in HTTP
transports advertise true because their wire formats support images. Configure a
text-only selected model explicitly, rather than inferring support from its name:

```toml
[[providers.local.models]]
id = "text-only-model"
image_input = false
```

This per-model setting applies through the provider registry, including live
reload and child bindings. Unsupported models receive a short note rather than
an image request. No model switch is implied. Library hosts using these adapters
outside the runner wrap their provider in `images::ImageProvider::new(provider,
store)`; the CLI binds the same resolution to every live provider generation.

Image references survive text-preview truncation and the generic tool-budget
footer. Their bounded metadata is appended outside `maxOutputTokens`; image wire
bytes are outside that text-preview budget. With the default 64 KiB dispatcher
limit, the 60 KiB text backstop leaves room for the four small references.
See [canonical attachment storage](storage-model.md#image-attachments-2026-10-15)
for replay and retention.

## Qualification (2026-10-13)

Every built provider kind meets the 12-item bar in [provider-qualification](provider-qualification.md) at the scripted and local-transport levels: `codex` and `responses` (Responses), `anthropic-messages`, `chat-completions` (now tool-capable), and the Claude Code extension. Live acceptance is a separate, ignored suite (`tests/provider_live.rs`) that skips without credentials. Its OpenCode Go kinds have been run live: chat completions and Messages pass, with a flaky chat schema test; Responses against Go is in the same results, see [provider-qualification](provider-qualification.md).

- **Codex OAuth.** BubblePaw never refreshes or writes `~/.codex/auth.json`; the Codex CLI owns refresh. The file is re-read on every attempt and again after a 401: a changed token gets one retry, an unchanged one an Auth error telling the user to refresh with `codex`. Missing or malformed files give Auth errors that name the path and never the token.
- **Published failures are not retried.** `ProviderError` carries `published` (set once text reached the listener; built with a builder, false by default) and `provider_error_retryable` returns true only for unpublished RateLimit and Transient errors. Every adapter sets it, and every retry goes through the registry's one `with_retry` loop, which uses the helper.
- **HTTP error bodies (2026-10-14).** Messages, Responses, chat completions and decision adapters use one diagnostic helper. HTTP rejections include the status, provider error type/code/message when present, and the sanitized response body. Reads stop at 64 KiB; displayed diagnostics stop at 4 KiB on a UTF-8 boundary and mark truncation. Request credentials, auth headers and secret fields are redacted before truncation. Error kinds, Retry-After and published-text retry rules stay the same. Stream error events retain their existing diagnostics.
- **Reasoning tokens.** `Usage.reasoning_tokens` is optional, omitted when unknown and summed when known. Responses fills it from `output_tokens_details.reasoning_tokens`, chat from `completion_tokens_details.reasoning_tokens`, the extension from the SDK's thinking-token count; Messages leaves it unset unless the usage object reports one.
- **Malformed tool calls** are metered and never executed on every kind.
- **Claude Code extension.** `output_schema` maps to the Agent SDK's `outputFormat` (`json_schema`) and `reasoning_effort` to `effort` with adaptive thinking on (unspecified means low). Both are checked against SDK 0.3.293 types. The host forwards them only to extensions whose `initialize` capabilities set `output_schema` / `reasoning_effort` to true and fails fast otherwise. Cache writes are reported as `cache_creation_tokens` (still included in `input_tokens`). Extension text is batched like the HTTP kinds. Auth, rate-limit, context and transient errors keep fixed messages. A fatal error (2026-10-25) keeps one redacted line of the underlying text, because the fixed message said nothing about what broke: the extension sends `Claude Code request failed: <line>`, and the host maps it to `extension request failed: <line>`. The line is the first nonempty one, whitespace-collapsed, cut to 300 characters after redaction. Redaction replaces bearer/basic tokens, `key|token|secret|password|cookie`-style fields, `NAME=value` env assignments, `sk-` keys, JWTs, opaque runs of 40+ characters and the values of credential-named env variables (the extension's own env; on the host, the instance's `env` entries). Claude Code's own stderr is logged the same way as `claude stderr: <line>`.

## Principles

- The harness loop stays the only loop. A provider does one model step: context and tools in, text and tool calls out. It never runs tools itself.
- The store stays the only history. A provider may keep a mirror (Claude Code's session file), but the harness tells it exactly when history changed; it never infers that by comparing.
- Provider-specific data travels as opaque `provider_state` on messages, already a field on `Message` and `ModelResponse`.

## Interface

`Provider::generate` stays the one required method. Additions, all with defaults so existing providers compile unchanged:

- `ModelRequest` gains:
  - `session_id: String`.
  - `history: HistoryCursor { epoch: String, rewritten_from: Option<String> }`.
- The harness computes the cursor centrally, after context preparation, so no context strategy has to report it:
  - It remembers, per session in memory, the prepared message list of the previous step as `(message id, content hash)` pairs.
  - If that list is a prefix of the new one, the epoch stays (a pure append).
  - Otherwise a new epoch (a fresh UUID) starts, and `rewritten_from` is the id of the first message that differs (`None` if the first message differs).
  - At harness start, and for a session's first step, there is no memory: new epoch, `rewritten_from: None`.
  - This covers summary checkpoints, repairs after abort or restart, edits and forks without touching their code.
  - Ephemeral messages: a prepared message whose id starts with `EPHEMERAL_ID_PREFIX` (`"ephemeral:"`, in `bubblepaw-core`) is optional host-provided per-step context. The cursor ignores it entirely: it is not remembered, not compared, and `rewritten_from` never names one. Stateful providers must not treat ephemeral messages as durable history; they may pass them on with the step that carries them. The runner's runtime inventory no longer uses this mechanism: initial/changed inventories are durable Generated-user `<harness kind="runtime_inventory">` reminders at the request tail, and unchanged inventories are quiet.
  - The summarizer gives legacy synthetic messages stable ids and timestamps from what they summarize (`context:checkpoint:<checkpoint id>`, `context:summary:<last legacy chapter id>`). Episode compaction uses the frozen view's exact rendered prefix as the summary block: its synthetic messages have `context:view:<rendered text fingerprint>` ids and timestamps from their coverage. The persisted prefix and masking choices remain unchanged between compactions, so background episode/chapter saves and runtime notes keep the epoch across steps. A changed rendered prefix or masking projection at compaction starts a new epoch.
- `fn capabilities(&self, model: &str) -> Capabilities`:
  - `context_window: Option<usize>` (feeds the summarizer's trigger),
  - `cache: CacheMode` (`None`, `Prefix`: the request prefix is cached server side, `Session`: the provider keeps a stateful mirror),
  - `tool_surface: ToolSurface` (below),
  - `stateful: bool` (wants `close_session` and the cursor).
  - `image_input: bool` (default false; whether the selected model accepts host image input).
- `async fn close_session(&self, session_id)`: the session was deleted or the provider changed; drop any mirror.
- Typed errors: `ProviderError { kind: Auth | RateLimit { resets_at } | Context | Transient | Fatal, message }`, so hosts can show "sign in again" or "limited until 14:00" instead of a string.

Stateless providers ignore the cursor. A stateful provider compares `(epoch, rewritten_from)` with its mirror: same epoch means append the new messages; a new epoch means rebuild from `rewritten_from`.

## Tool surface

The registry already separates model tools from host tools (reachable only through a tool's dispatcher, as `js` uses). A provider picks how tools are presented:

- `Direct`: every model tool is offered as a tool (Codex, API providers).
- `Code`: the model sees only `js`; all other tools live in `tools.*`, discovered with `tools.$search`. A few always-shown tools get full signatures in the `js` description; the rest are listed by name. For Claude Code each direct tool call is an MCP round trip through a parked call, so batching through `js` matters.

In opt-in JS execution mode, the harness applies the provider's surface per
step and declares only `js`. Native execution is the default for new chats,
takes precedence over this legacy preference, and never declares `js`. The
extension transport does not filter the list again; standalone `Provider`
callers supply their desired declarations in `ModelRequest.tools`.
See [execution modes and embedder APIs](codemode.md).

With `--web`, the runner also registers always-shown `research` and `web_fetch`
host capabilities; `web_search` requires explicit exposure. Their signatures
appear in the generated JS reference. A research child can select another
`provider/model` through its profile's model override; otherwise it inherits the
parent's active run model, including a per-run override. See [web tools and profiles](web.md).

## Several providers

- The harness holds a provider registry keyed by provider id (`codex`, `anthropic`, `claude-code`, `deepseek`, `scripted`). A model is named `provider/model`; a session has a default, and a run may override it.
- Switching a session to another provider keeps the history; the new provider gets a fresh cursor (its mirror, if any, rebuilds). The old one gets `close_session`.
- Changing models on the same provider preserves that provider's mirror/cursor. Fallback selection is tracked separately by source session and requested model. Native replay still applies model/route compatibility guards; this is not a promise of cross-model cache reuse.

Built-in role adapters use `ProviderEndpoints` in the runner's
`src/provider/endpoints.rs`. It owns opencode chat's optional base URL,
`OPENCODE_API_KEY` environment name and `thinking`/`reasoning_effort` fields
(OpenCode Go quota does not use that key: it needs a separate console login, see
[quota](quota.md#opencode-console-login));
OpenRouter decisions' URL and `OPENROUTER_API_KEY` environment name; and OpenAI
decisions' base URL and `OPENAI_API_KEY` environment name. `register` validates
these settings and adds all adapters whose keys are present to the shared
`ProviderRegistry`. `register_with_keys` accepts explicit `ProviderKeys` for
isolated tests and replay's local relay. Empty or missing keys skip registration.
These settings are separate from `ModelRoles`, which episodes and approvals use
to choose models. The legacy `--episode-summary-base-url`,
`--episode-summary-key-env` and `--episode-summary-reasoning-fields` flags (and
their `BUBBLEPAW_EPISODE_SUMMARY_*` environment settings) configure the opencode
endpoint; an empty base URL disables that adapter. See
[decision-providers.md](decision-providers.md) for decision roles and calibration.

## Chat completions (2026-10-13)

**Status:** built; local qualification uses synthetic transport tests. OpenRouter
chat continuation and tool reasoning replay pass live on 2026-10-14; see the
per-provider results in [qualification](provider-qualification.md).

`chat-completions` is a stateless, direct-tool main provider and a model-role
transport. The registry and primary model selector accept configured chat
provider/model addresses; see [configuration](config.md) for a complete example.
Authentication uses `Authorization: Bearer <key>`, read from the configured
environment variable at provider construction.

Offered tools use OpenAI function definitions with `name`, `description` and
`parameters`. The core request has no tool-choice or parallel-call controls,
so both wire fields are omitted and the endpoint chooses its defaults. Streamed
`choices[0].delta.tool_calls` fragments are joined by index, including split
IDs, names and argument JSON. Several calls can finish in the same turn. Native
IDs become core tool-call IDs, as in Responses and Messages. Malformed argument JSON,
duplicate IDs and unoffered calls fail with metered usage before execution.
An error `finish_reason` also fails the turn, including streams that end with
`[DONE]` and have no top-level error object. A bounded native reason code such as
`MALFORMED_FUNCTION_CALL` appears in the diagnostic. Reported usage, including
explicit zeros, remains available; partial streamed text marks the error as
published and prevents retry. The same rule applies to JSON responses.

Assistant history carries `tool_calls` alongside text (or null content for a
call-only turn); tool results carry `tool_call_id` and content. These fields
remain when the host offers no further tools. Reasoning replay follows the
model's `none`, `tool_loop` or `all` policy (default `tool_loop`) and the origin
provider/model guard.
`tool_loop` sends the current loop's reasoning with its assistant call turns.
Streamed `reasoning_content` and OpenRouter `reasoning` are accumulated as
separate strings. OpenRouter `reasoning_details` arrays are concatenated in
arrival order; their objects, including repeated indices, signatures and
encrypted data, are preserved unchanged. When an endpoint sends both plain
reasoning and details containing the same text, both fields are stored and
replayed. See [OpenRouter's reasoning and streaming contract](https://openrouter.ai/docs/guides/best-practices/reasoning-tokens).
Unknown stream extensions remain in the opaque journal without an invented
assembly or replay rule. These rules apply to every completed continuation
round and JSON fallback; failed or cancelled streams keep no assistant state.
Standard SSE completion metadata (`id`, `object`, `created`, `model`,
`system_fingerprint`, `service_tier`) is retained once in response metadata,
rather than copied into the opaque journal for every token. A differing later
value remains in the ordered journal, as do repeated unknown fragments.
Prepared-request accounting excludes the recognized Chat journal because it
is archival and is never replayed; native message fields remain counted.
Text-only no-tools wire bodies remain byte-identical, frozen by a
golden test committed before the adapter change.

HTTP 401/403 maps to Auth; 429 keeps Retry-After and its derived `resets_at`;
400 context overflow maps to Context; 408/409/5xx and connection failures map
to Transient. Error chunks inside a stream are classified without returning
server error text. A failure after listener text is published is fatal and is
not retried. Retry and backoff cancellation belong to the host. An output-limit
stop drops cut calls before continuation and keeps each round's reasoning once.

Usage maps prompt and completion tokens, cache reads (including DeepSeek
`prompt_cache_hit_tokens`), reported cache creation tokens and
`completion_tokens_details.reasoning_tokens`. Known reasoning counts sum over
continuations. Request effort reaches `reasoning_effort`; the default is low.
Structured output uses the existing native `response_format` schema mapping.

The local bar in [provider qualification](provider-qualification.md) is covered
by `provider_chat.rs`, `provider_stream.rs`, `provider_continuation.rs`,
`provider_retry.rs`, `provider_structured.rs`, `reasoning_replay.rs` and the
Chat cases in the byte-exact `reasoning_storage.rs` audit. These checks use
loopback fixtures only. The live gate remains outside this scope.

## Anthropic Messages (2026-10-09)

**Status:** Built. Summary and one tool round trip passed live against OpenCode
Go on commit `59115cf`, as reported from an earlier session. Follow-up changes use local
transport and scripted verification; no further live calls were made.

`MessagesProvider` is a stateless HTTP adapter that streams its reply (see
Streaming below), addressed by default
as `opencode-messages/claude-haiku-5-5`. It posts to
`https://opencode.ai/zen/go/v1/messages` with `x-api-key: OPENCODE_API_KEY`,
`anthropic-version: 2023-06-01` and JSON content type. Config aliases use kind
`anthropic-messages`, their own `base_url` and `key_env`. The base includes `/v1`;
the adapter appends `/messages`. No SDK dependency is needed.
It also sends its own user agent and a stable `x-opencode-session` header, as
requested by the [OpenCode Go client guidance](https://docs.opencode.ai/docs/go/).

Every request uses `thinking: {type: "adaptive"}` and
`output_config: {effort: "low"}` by default. A role/request effort replaces the
output-config effort while thinking stays adaptive. There are no sampling parameters or assistant
prefills. `max_tokens` is required, so it is always sent: the role's `max_output_tokens` when set, otherwise the model maximum from the table in `provider/messages.rs`, where `claude-haiku-5-5` is 128,000 (source in Output length below). A length stop is continued the same way. Only the request's system prompt goes
in the top-level `system` field, so it stays byte-identical across an LCM apply.
Every history system message (LCM view blocks, cold notes, any mid-list control)
renders in place as a user text block, merged into adjacent user content like
consecutive user turns (P5, 2026-10-21). In a user turn, `tool_result` blocks
always come first, so text from a system or user message never precedes a result.
Tools use Anthropic
definitions and automatic choice; the harness executes the returned `tool_use`
calls. Consecutive tool results become blocks in one user message, including
`is_error` for failed calls.

The complete assistant content array is kept in
`provider_state = {adapter: "anthropic-messages", content: [...]}` and replayed
unchanged on later steps, including signed thinking and redacted thinking.
Reconstructing an assistant tool turn from text and calls loses thinking when
compatible provider state is unavailable (`provider/messages.rs:97`, request body);
the adapter rejects that fallback in the current tool loop with a local
error identifying missing or incompatible state, instead of sending an invalid
thinking-less request. Generated runtime host notices keep that loop open;
an actual new user task or completed assistant answer closes it. Older
completed/imported turns can use the fallback.
Existing assistant turns are not edited. Ephemeral cache breakpoints (at most
four) sit at the context tier ends, placed by one function
(`provider/messages_cache.rs`) from `ModelRequest::cache_tiers`: the frozen end
(the system block, else the last tool), the cold end, the previous request's
tail and the last block. Without tiers only the frozen end and the last block are
marked. Message breakpoints go on user-turn blocks only, never on an assistant's
preserved blocks. See prefix-cache-design.md, P5. The cache is a
disposable prefix cache; authoritative history remains in SQLite.

Input-token totals include uncached input, cache reads and cache creation,
matching the existing whole-prompt usage convention. Cache reads are recorded
in `Usage.cache_read_tokens` and cache creation in `Usage.cache_creation_tokens`.
Both are numeric, default to zero and accumulate in run/summary usage. Old JSON
and rows with omitted fields or a null cache-read value still load. These cache
counts are subsets of the whole input total, so callers must not add them again.
The raw creation count remains in provider state for compatibility.
`refusal`, missing/unknown stop reasons and malformed
responses are explicit metered errors. A `max_tokens` stop is not an error: it is continued (see Output length below). HTTP errors and cancellation use the existing typed errors.

`ProviderEndpoints` has separate Messages URL and key-env settings. Its
`register_with_keys` uses the explicit OpenCode key for both OpenCode adapters;
environment-based registration reads each adapter's configured key-env.
Replay's pricing, model allow-list and HTTP relay are separate work owned separately
and are not Messages acceptance evidence.

The registry, roles and CLI primary-provider selection recognize Messages.
`--model opencode-messages/claude-haiku-5-5` or the same `model.main` config value
works without Codex credentials. Decision consumers call the existing
`DecisionProvider::decide` route with model
`llm/opencode-messages/claude-haiku-5-5`. For a named role, configure
`roles.summary=["opencode-messages/claude-haiku-5-5"]` and
`roles.decider=["llm/summary"]`. The LLM decision adapter uses its existing JSON
prompt/parser with no tools; each answer is a hard 0/1 vote. Repeats give vote
share, which is not calibrated probability. Luna/Jev remain the calibrated
decision routes.

Direct Anthropic uses the same `anthropic-messages` kind: configure
`base_url="https://api.anthropic.com/v1"`, `key_env="ANTHROPIC_API_KEY"` and an
appropriate model address under that provider alias. No extra adapter is needed.

`ModelRequest.reasoning_effort` is optional for older callers; omission uses
the configured provider effort or `low`. Role requests always supply their
effort. Responses maps it to `reasoning.effort`, chat to `reasoning_effort`, and
Messages to `output_config.effort`. LLM decisions carry the role effort through
`DecisionRequest`; direct decisions default to low. Effort is one of `low`,
`medium`, `high`, `xhigh`, `max`; disabled/none is rejected. A model may reject
an effort value it does not support; adapters do not silently reduce it.

Local mock tests cover request shape, parallel tool results, preserved thinking,
usage and stop errors. Follow-up live acceptance is unverified. From this worktree, with
`OPENCODE_API_KEY` already set, these opt-in checks can be run explicitly:

```powershell
# One short summary request, no tools (one model call).
cargo test -p bubblepaw-runner --test provider_messages live_summary_smoke -- --ignored --nocapture
# One echo-tool round trip (two model calls).
cargo test -p bubblepaw-runner --test provider_messages live_tool_smoke -- --ignored --nocapture
```

These tests are ignored by normal workspace tests. They make real model calls
only when explicitly selected with `--ignored`; they were not rerun for the follow-up.

Verification on 2026-10-09 in the `anthropic-messages` worktree: `cargo fmt --all`,
`cargo clippy --workspace --all-targets -- -D warnings` and
`cargo test --workspace` passed (403 tests passed, two live smoke tests ignored).
Eight local adapter tests cover the Messages transport; the CLI integration
test verified the provider kind, summary/chapter role configuration and local
registry dispatch. That count records the initial adapter gate; the follow-up
also adds numeric cache persistence, role effort and decision-routing tests.
Replay pricing and allow-list acceptance remain separately owned.
Follow-up gates on the same date passed: formatting, strict workspace Clippy,
and `cargo test --workspace -- --test-threads=1` (415 passed, two live tests
ignored). The default parallel workspace attempt failed in the unrelated Windows
descendant-cancellation test with an empty PID; that unchanged test passed in
isolation and in the full serial run. CLI local mocks verify primary Messages
selection through both `--model` and `model.main`.
Replay imports recorded assistant turns without calling the selected primary
transport. CLI/config selection of a Messages main now resolves role references
to its concrete address. A dry replay with supported auxiliary roles works;
Haiku auxiliary roles remain rejected by the existing metering allow-list.
This follow-up does not claim live Haiku replay acceptance.

## Streaming (2026-10-10)

**Status:** Built with mock SSE tests (`tests/provider_stream.rs`, `tests/provider_responses.rs`).
No live call has been made for streaming; the smoke command is run only after approval.

All three HTTP kinds (Responses, chat completions, Anthropic Messages) send
`stream: true` and read server-sent events. Each assembles the same complete
`ModelResponse` as before: text, tool calls with their JSON input joined from
split deltas, Messages thinking blocks with signatures kept byte-identical,
usage including cache reads and writes, and the stop reason. Chat requests
`stream_options.include_usage`.

- The idle timeout is 300 s per read (`provider/stream.rs` `IDLE_TIMEOUT`). The
  old 300 s whole-request timeout is gone, so a long answer can finish.
- Text reaches the provider listener in batches: at 512 characters, or 75 ms after
  the first pending fragment, and at each block end and the terminal event.
  Each batch is forwarded to host subscribers as a live `text_delta` progress
  event (`EventKind::TextDelta`): never stored, not part of `streaming_text`,
  and not counted in `live_seen`. The `paragraph` events are unchanged.
- Nothing partial is persisted. A cancelled call returns `cancelled`; a stream
  that fails mid-way returns its error and keeps what the listener already got.
- A dropped or silent stream is transient only before any text reached the
  listener. After that it is fatal, since a retry would repeat text. Every
  adapter makes one attempt per call; the registry's single retry policy
  (below) decides whether a failed attempt is retried.
- A reply without `text/event-stream` is read as one JSON body, as before, so
  non-streaming mocks still work.
- Thinking deltas are accumulated into the provider state but not sent to the
  listener; there is no thinking channel yet.
- No output-size cap in the HTTP adapters: output length never rejects a reply.
  Two memory-safety limits remain, and they are not output-size limits: 32 MiB
  per streamed body (`MAX_STREAM_BYTES`) and 8 MiB per unparsed SSE frame
  (`MAX_FRAME_BYTES`), both in `provider/stream.rs`.

Live smoke, to run only after approval (one short streamed Haiku call,
`opencode-messages/claude-haiku-5-5`, needs `OPENCODE_API_KEY`):
`cargo test -p bubblepaw-runner --test provider_messages live_summary_smoke -- --ignored --nocapture`

## Output length (2026-10-10)

**Status:** Built with mock tests (`tests/provider_continuation.rs`). No live call was made.

Output length never fails a model call. The defaults send no output limit where the API allows one:

- Responses and chat completions send no output-token field unless the role sets `max_output_tokens` (Responses `max_output_tokens`, chat `max_tokens`).
- Messages always sends `max_tokens`, from the model table in `provider/messages.rs` (`claude-haiku-5-5`: 128,000), unless the role sets a value. The 128K figure is from the Anthropic models overview, `https://platform.claude.com/docs/en/models/haiku-5-5/overview` ("Max output: 128K tokens"); 128000 is our reading of 128K. Models outside the table use 64,000, our fallback rather than a documented value.
- `roles.<role>.max_output_tokens` (see `config.md`) sets the limit in all three APIs. Decision providers (`decisions.rs`) send no per-role limit yet.

A length stop is continued by the shared helper in `provider/continuation.rs`. Each API reports it differently, but every continuation sends the same two messages: the partial reply as an assistant turn, then a user turn "Continue exactly where you stopped." Prefilled final assistant turns are never sent, because Claude 4.6 and later reject them with a 400. Since 2026-10-13 the partial turn carries round one's reasoning: Responses sends the round's native output items (reasoning, then the partial message) and Messages its native content blocks (thinking, redacted thinking, text) in their original positions; chat sends the text only, or text plus `reasoning_content` when the model's `reasoning_replay` is `all`. The final stored reply holds the reasoning of every round exactly once.

- Messages: `stop_reason` `max_tokens`.
- Responses: a `response.incomplete` event whose `incomplete_details.reason` is `max_output_tokens`. It ends the reply; it is not an error.
- Chat completions: `finish_reason` `length`.

Rules:

- Up to 4 continuations (5 requests in total). Text joins across the calls, and usage (input, output, cache reads and cache creation) sums across them.
- A cut inside a tool call drops the partial call and continues from the text before it. A reply that ends with complete tool calls is not continued, because a continuation would need their results. It is returned with its tool calls and logs a warning.
- If the reply is still cut after 4 continuations, it returns what it has, marked incomplete by a `tracing` warning (target `bubblepaw::provider`). That is not an error. There is no typed incomplete field yet.
- Refusals stay errors (Messages `stop_reason` `refusal`). Stream failures are not continued and keep their error kind.
- Episode summaries have no size backstop. The old 64 KiB check in `episode_models.rs` is removed.

## Reasoning artifacts (2026-10-13)

**Status:** built with loopback mock tests; no live call was made.

Every adapter stores the reasoning it receives; see the storage rule in
[storage-model](storage-model.md#reasoning-artifacts). Replay is decided per
request by the adapter, in `bubblepaw_core::provider_state_replay_compatible`:

- **Responses** sends a stored `reasoning` item (with `encrypted_content`) only
  when the state's adapter is `responses` and its origin model equals the request
  model. **Messages** does the same for `thinking`/`redacted_thinking` blocks with
  `anthropic-messages`. Claude Code replies are tagged `anthropic-messages` with
  the Claude Code model id.
- Incompatible artifacts stay in the store. Older turns go out as plain text
  and tool calls, and a reasoning-only record (from an import) adds nothing.
  Messages rejects missing/incompatible state on current-loop assistant tool
  turns because reconstructing those turns can lose required signed blocks.
  When the session returns to the origin provider and model, the
  artifacts are sent again unchanged (`tests/reasoning_replay.rs`).
- States without `_bubblepaw_route_origin` inherit the current route, provided
  their adapter/model are compatible, so plain upgrades preserve unfinished signed
  Messages tool loops. Only a recorded route/endpoint mismatch is incompatible;
  native adapter/model replay checks still apply.
- **Chat completions** stores `reasoning_content`, OpenRouter `reasoning` and
  ordered `reasoning_details`, and other opaque response
  fields from every round, and replays them per model:
  `reasoning_replay = "none" | "tool_loop" | "all"`, default `tool_loop`, which
  sends compatible current-loop reasoning and retains canonical tool calls and
  results. `none` and `all` remain explicit settings
  ([config](config.md#chat-reasoning-replay-2026-10-13)).
- **Continuations** carry the partial reply's reasoning (see Output length).
  Signed blocks always go back. A Messages thinking block cut by `max_tokens`
  before its signature is incomplete: it is stored with the reply, marked
  `unsigned_cut`, and never sent to the provider, in a continuation, a later
  tool round or a later turn.
- **Failed streams:** partial reasoning of a stream that fails or is cancelled is not kept (decision 2026-10-14),
  because the response never completed. A retry produces fresh reasoning.
- **Compaction:** reasoning of turns a summary replaced stays stored but is not
  replayed, consistent with the summary replacing those turns. Rewind, branch
  and LCM expansion use the stored originals.
- **Claude Code extension:** thinking with `signature` and `redacted_thinking`
  with `data` are captured from the stream (and from the non-streaming fallback
  and one-off steps) into `provider_state` as Messages-format content with text
  and tool calls as references. A rebuild writes them back into the Claude Code
  session in their original order when the model matches; on a model change the
  mirror rebuilds without them. On `reuse`, Claude Code resumes its own session
  file, which already holds the thinking blocks it wrote.

## Structured output (2026-10-09)

`ModelRequest.output_schema: Option<JsonSchema>` requests native structured
output, with `JsonSchema { name: String, schema: serde_json::Value }`. Omission
preserves ordinary text requests and compatibility with older serialized
requests. `JsonSchema::new(name, schema)` validates immediately;
`JsonSchema::validate()` and `ModelRequest::validate_output_schema()` are also
available when a host builds or deserializes the public fields directly.

Every object schema must explicitly set `additionalProperties: false`, including
nested properties, array items, composition branches and definitions. Errors
name the schema path. Schema names use 1-64 ASCII letters, digits, underscores
or dashes. All three HTTP adapters validate before sending a request.
This is a closed-object check, not complete validation of each provider's
supported JSON Schema subset; a provider can still reject unsupported schemas
or models. External extensions receive the optional field through serde and
must implement their own support.

| Adapter | Request field |
|---|---|
| Anthropic Messages | `output_config.format = {type: "json_schema", schema}` |
| Responses | `text.format = {type: "json_schema", name, schema, strict: true}` |
| Chat completions | `response_format = {type: "json_schema", json_schema: {name, schema, strict: true}}` |

These mappings follow the [official OpenAI documentation](https://developers.openai.com/api/docs/guides/structured-outputs)
and [Claude structured output documentation](https://platform.claude.com/docs/en/build-with-claude/structured-outputs).
Messages has no format-name field, so the core name is only sent by the OpenAI
formats. Structured output preserves the existing effort path: an explicit
request effort wins, otherwise the provider's configured effort or low applies.
Messages retains adaptive thinking. Disabled/none is not an accepted effort.

`ModelResponse::parse_json<T: serde::de::DeserializeOwned>() -> anyhow::Result<T>`
deserializes the complete text into a host-owned Rust type and reports malformed
JSON or type mismatches. It rejects responses carrying tool calls. This helper
uses serde's contract, including `#[serde(deny_unknown_fields)]` when the host
wants it; it does not independently validate the response against JSON Schema
or retry a refusal or incomplete reply.

The ordinary harness also accepts `RunOptions.output_schema`. It forwards the
native format on provider requests and validates a final JSON reply against the
schema. An invalid final reply gets one tools-disabled correction containing the
validation error; a second invalid reply fails the run. Script-only runs reject
`output_schema`, because they do not make a provider request. These generic run
controls support [child structured answers](child-sessions.md#structured-output-tool-budgets-and-timeout)
without adding child orchestration to the engine.

Run controls keep the Messages thinking prefix stable (2026-10-14). A tool-call
budget is a `tool_budget` reminder, a generated user message. It is sent once
at run start with the limit and remaining count, and once more as a warning when
remaining calls drop to max(2, 20% of the limit) or fewer. Tool results also end
with the remaining-calls footer. A run without a budget gets no budget reminder.
The first tools-off round (budget exhausted or schema correction pending) keeps the
active `tools` unchanged and queues a `tools_off` reminder, so it sends no tool-choice
change on any adapter. Only if the model still calls a tool does the next round
(the enforcement round) move those declarations into `disabled_tools`. Every adapter
then keeps them on the wire, in the same order as earlier rounds, and turns the
provider's tool choice off: `tool_choice: {type: "none"}` for Messages,
`"none"` for Chat Completions and Responses. Call parsing still accepts only active
tools, so a call on a tools-off round is rejected; a refused call gets an error
result and is not executed.
The metadata is transient and does not enlarge saved provider state. See
[thinking replay](thinking-tool-loop.md) for the recorded live rejection and fix.

Offline tests in `structured_output.rs` and `provider_structured.rs` cover serde
compatibility, nested validation, exact request shapes, effort defaults and
overrides, ordinary-request omission, rejection before HTTP and mock typed
round trips through all three adapters. The dedicated research-child probe in
[thinking replay](thinking-tool-loop.md) also exercises native structured output
live; the separate one-call smoke below remains unrun.

Explicit opt-in acceptance (one paid model call, not run during offline gates):

```powershell
cargo test -p bubblepaw-runner --test provider_messages live_schema_smoke -- --ignored --nocapture
```

`provider::parse_retry_after` accepts delta-seconds and HTTP dates; it is the
only Retry-After parser, and `provider/http_error.rs` maps HTTP statuses to error
kinds for every adapter.
`provider::provider_retry_after(&anyhow::Error)` exposes the remaining server
delay through the error source chain, preserving the core typed error.
Messages, chat and Responses retain these hints for retryable HTTP failures
(`408`, `409`, `429`, and `5xx`), including `503`.

One retry policy (`RetryPolicy` in `provider/retry.rs`: 4 attempts, 500 ms
initial backoff doubling with 0.5â€“1.0 jitter, capped at 15 s) runs every call
through `ProviderRegistry`: main turns, decisions, auxiliary role calls and
summary forks. `set_retry_policy` and `with_retry_policy` replace it;
`RetryPolicy::once()` makes one attempt. A failed attempt is retried only while
attempts remain, the call is not cancelled, no text was published, the error is
retryable, and it is not a terminal or quota stop. Rate limits are left to the
quota host when one drives the call, and an exhausted quota window is never
retried inline. A server wait is honored up to `max_delay`; a longer wait
returns at once with its Retry-After attached, and the quota host or the caller
owns it. A route is retried per the policy before the next fallback route is
tried. Local tests: `provider_main_retry.rs` (a main turn retries once; published
output is never retried), `provider_retry.rs` (long waits are not retried
inline) and the unit tests in `provider/retry.rs`.

The native quota host (2026-10-20) intercepts 429s before the registry's inline
retry sleeps, persists returned meters and applies the short/exhausted/terminal policy.
This preserves transport retry behavior for direct adapter consumers while
giving hosted runs durable wait markers. Billing/spend caps and explicit
non-retryable quota failures are terminal. See [quota](quota.md).

## Out-of-process extensions

JSON-RPC 2.0, one record per line over stdio, like the runner's `bubblepaw stdio`. The host spawns the extension on first use and restarts it after a crash.

| Method | Direction | Meaning |
|---|---|---|
| `initialize` | host to extension | Returns provider id, models, capabilities per model. |
| `step` | host to extension | A `ModelRequest`. Result: a `ModelResponse` or a `ProviderError`. |
| `text` (notification) | extension to host | Streamed text for the running `step` id. |
| `cancel` (notification) | host to extension | Cancel the `step` with this id. |
| `session.close` | host to extension | `close_session`. |

Shapes (the serde forms of the core types):

- `initialize` result: `{"provider": "claude-code", "models": [{"id", "name", "capabilities": {"context_window", "cache": "none"|"prefix"|"session", "tool_surface": {"kind": "direct"} | {"kind": "code"}, "stateful"}}]}`.
- `step` params: a `ModelRequest`: `{"model", "system_prompt", "messages": [Message], "tools": [ToolDefinition], "max_output_tokens", "reasoning_effort"?, "output_schema"?: {"name", "schema"}, "session_id", "history": {"epoch", "rewritten_from"}}`. `Message` is `{"id", "session_id", "role": "system"|"user"|"assistant"|"tool", "content", "tool_calls": [{"id", "name", "arguments"}], "tool_call_id", "provider_state", "is_error", "created_at"}`.
- `step` result: `{"text", "tool_calls": [..], "usage": {"input_tokens", "output_tokens", "cache_read_tokens", "cache_creation_tokens"}, "provider_state"}`. Missing cache fields default to zero for older extensions. An optional `reported_model` is the full model ID the upstream reports for the step (the Claude Code extension sends the SDK's ID); usage rows are named by it, and the configured model id is the fallback when it is absent or not a short identifier. Failure: a JSON-RPC error whose `data` is `{"kind": "auth"|"rate_limit"|"context"|"transient"|"fatal", "resets_at"?, "usage"?, "reported_model"?}`. `usage` is the reported usage of the failed call (same shape as a step result's), so it is still metered; `reported_model` names its row as above.
- `text`: `{"method": "text", "params": {"id": <step request id>, "text": "..."}}`. `cancel`: `{"method": "cancel", "params": {"id": <step request id>}}`; the step then fails with kind `transient` and message `cancelled`.
- `session.close` params: `{"session_id"}`.
- Hosts configure extensions in a JSON file, `[{"id": "claude-code", "command": "bun", "args": ["run", "src/main.ts"], "cwd": "<repo>/extensions/claude-code", "env"?: {"NAME": "value"}}]`, or in `[extensions.<id>]` config tables. `env` is set on the child on top of the inherited environment, and `BUBBLEPAW_EXTENSION_ENV_KEYS` lists its names.

The extension host runs in its own process tree (`bubblepaw-process`: a process group on
Unix, a kill-on-close Job Object on Windows). Replacing or dropping the connection kills that
tree and aborts the writer, reader and stderr tasks, so nothing the host started outlives it.

After a restart the host sends the next step with `rewritten_from: None`, so a stateful extension rebuilds.

## Claude Code provider (first extension)

A Bun process on the Agent SDK and cc-session-io, following pi-claude-bridge:

- Claude Code runs with no built-in tools, no setting sources, CLAUDE.md excluded, claude.ai MCP servers off, strict MCP config, auto-compaction off.
- The extension serves the request's tools to Claude Code from an in-process MCP server. A `tools/call` is parked; the step returns that tool call; the next `step` carries the result (as a tool message) and releases the parked call. Pairing uses Claude Code's `claudecode/toolUseId` metadata.
- Session file sync from the cursor: append (reuse) or rebuild. Native assistant content (thinking, signatures, redacted thinking, text and tool-use order) is kept in `provider_state` and written back verbatim on rebuild (2026-10-13). Planned improvements, each behind a probe test: tail-only rebuilds, adopting records Claude Code wrote that the store lacks. Measure cache reads per step.
- Summaries for compaction can run through the same extension as a one-turn, tool-less step.

### Claude subscriptions

Built 2026-10-19; live qualification pending (the limit error itself was seen live). The extension runs on whatever account Claude Code is logged in to; BubblePaw handles no credentials.

- **Launch.** `command = "bun"`, `args = ["run", "src/main.ts"]`, `cwd` = the extension directory (Bun in `~/.bun/bin` on Linux). The SDK spawns its bundled native Claude Code binary (`node_modules/@anthropic-ai/claude-agent-sdk-linux-x64/claude` on Linux); no `claude` on PATH is needed.
- **One instance per subscription.** Configure one extension per account, e.g. `claude-code` and `claude-code-b`, each with its own `env.CLAUDE_CONFIG_DIR` (see [config.example.toml](config.example.toml)). Claude Code reads that dir's `.credentials.json` and writes its session files there.
- **Login** happens once per config dir, with Claude Code itself, in an interactive terminal on the machine that runs BubblePaw (over SSH, use `ssh -t <host>`):

  ```sh
  cd BubblePaw-Core/extensions/claude-code
  CLAUDE_CONFIG_DIR=$HOME/.claude-b node_modules/@anthropic-ai/claude-agent-sdk-linux-x64/claude auth login --claudeai
  CLAUDE_CONFIG_DIR=$HOME/.claude-b node_modules/@anthropic-ai/claude-agent-sdk-linux-x64/claude auth status
  ```
- **No silent API billing.** The SDK's `env` option replaces the child environment, and the extension builds it from its own environment minus `ANTHROPIC_API_KEY` and `ANTHROPIC_AUTH_TOKEN`, unless the instance's `env` table sets them (the host lists explicit names in `BUBBLEPAW_EXTENSION_ENV_KEYS`). An inherited `CLAUDE_CODE_OAUTH_TOKEN` is not removed; if set, it may override the config dir's login, so keep it out of the runner's environment.
- **System prompt mode.** `BUBBLEPAW_CC_SYSTEM_PROMPT` in the instance env: `preset-append` (default) appends BubblePaw's prompt to Claude Code's `claude_code` preset; `replace` sends BubblePaw's prompt alone as a plain string. Other values stop the extension at start.
- **Native quota (2026-10-20).** The extension forwards both successful and rejected SDK rate-limit events, preserving utilization, status, type, reset and unified/overage fields. Provider and extension entries accept an `account` identity, defaulting to their entry name. The host also reads the subscription usage endpoint on demand using the existing credentials file; it never refreshes or writes that token. Exhausted windows wait in place within `[quota] max_wait_seconds`; a small durable marker lets a restarted host schedule the existing new-run resume path. Spend/billing failures and waits beyond the maximum fail immediately. See [quota](quota.md) for source evidence, timing, storage and restart semantics.
- **Configured failover.** Explicit `model.fallbacks` remain available for eligible failures after quota policy. Quota does not rotate or select accounts. Existing fallback provenance and sticky-route behavior remain; switching to another extension resets its cursor and rebuilds its Claude Code session from history.

## Open

- Steering mid-generation (outside a parked tool call) is not done; steering reaches Claude Code at the harness's tool-round boundary, through the prompt stream (above).
- Approvals (a tool call waiting for a decision) are built as step 1.4; see [approvals.md](approvals.md).

## Live configuration (2026-10-14)

The runner's `provider::LiveProviderConfig` owns immutable generations. Embedders
share its `Arc` as their `Provider`, bind setup with `ProviderSetup::with_live`,
and bind each request through `HarnessConfig.provider_resolver`. Explicit
`RunOptions.model` overrides stay pinned. `HttpEpisodeModels::from_live` and the
live store-aware web helpers use the same generation; ordinary constructors
remain available for fixed configurations. `AutoApprovalConfig.resolve` captures
one complete approver model/threshold configuration at each approval gate.

Long-lived web and stdio hosts apply provider setup changes immediately. External
file edits apply at the next turn's mtime/size check, or immediately through
`tools.reload`, `/reload`, `POST /api/config/reload` or stdio `config.reload`.
There is no file watcher. The replacement is validated and prepared before
it replaces active providers and model roles. Credential environment variables
resolve at apply time without exposing their values. Invalid configuration keeps
the active registry intact.

An in-flight request finishes with its previous provider; the next request or
tool round uses the new configuration, including in existing conversations,
children and workers. Explicit routes remain pinned. Removed provider/model pins
fail on the next request instead of silently rerouting. Persisted history and
reasoning remain intact; replay origin checks cover adapter/model, configured
provider route and endpoint when recorded. A recorded different endpoint cannot
receive those opaque artifacts; absent route tags inherit the current route while
native adapter/model compatibility checks still apply.

Startup and reload share web setup validation: HTTPS except machine loopback,
base URLs without credentials/query/fragment, and key-variable identifiers ending
in `_API_KEY`, `_KEY` or `_TOKEN` or explicitly allowed by `[web] provider_key_envs`.
Named model roles (including effort and approver threshold) and web search/fetch
backends, credential variable and cooldown also apply live. The runner's
`LiveConfig` coordinates those generations with live approval, agents, exec,
web exposure/policy and MCP inventory; core only provides generic turn/registry
and policy hooks. Storage, context, web caches and extensions remain restart-only. See
[configuration](config.md#live-configuration-2026-10-14), [live config](live-config.md) and the
[API payload](web-contract.md#provider-setup) for exact scope and status fields.

## Local chat servers (built 2026-10-15)

The existing `chat-completions` kind also registers loopback endpoints without
`key_env`. These requests omit Authorization entirely. Other HTTP kinds and
non-loopback chat routes retain their credential requirements; a configured
key reference never silently falls back to unauthenticated access. The
[llama-server example](config.md#local-llamacpp-server-built-2026-10-15) uses
`http://127.0.0.1:8080/v1`, with no new provider kind. Config registration and a
local HTTP fake are tested; no live local-model run is claimed.
