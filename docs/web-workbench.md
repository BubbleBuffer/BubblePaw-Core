# Web workbench: older history and provider setup

Status: approved and built 2026-10-13; live provider apply added 2026-10-14. Closes the two deferred gaps in
[web contract](web-contract.md) and README "Current limits".

## Control tools (2026-10-15)

The session view includes the latest todo list, shown as a small progress
disclosure, and one pending question. Questions show suggested answers and a
free-text input. Failed answer requests keep the typed text. A restored pending
question shows “Waiting for answer” and retains Stop even without an active run.
Answers use `POST /api/sessions/{id}/question/answer`; state changes arrive on the
existing SSE stream. See [session API](session-api.md) for bounds and recovery.

## 1. Older-history pagination

`GET /api/sessions/:id` returns the newest 200 messages (`store.snapshot`,
`ORDER BY seq DESC LIMIT`, only those rows decoded). The messages endpoint below
provides bounded reads of earlier history.

**Core (one generic read).** `Store::messages_before(&self, session: &str, before_id: Option<&str>, limit: usize) -> Result<Option<Vec<(i64, Message)>>>`
next to `messages_range` in `store_cold.rs`. It routes through `scoped` like the other
phase C readers, selects `seq < (seq of before_id)` with `ORDER BY seq DESC LIMIT`,
decodes only those rows and returns them ascending. `None` reports an unknown cursor;
`Some` holds the page, including an empty page. Cursor lookup uses the existing
`messages_id(session_id, message_id)` index. The cursor is a message id, not a
seq: seqs are global per database file and change when a chat is archived or restored;
ids are stable.

**Host API (additive).** `GET /api/sessions/:id/messages?before=<message id>&limit=<1..200, default 100>`
returns `{ messages: Message[], has_more: boolean }`, oldest first, `provider_state`
stripped as in `SessionView`. The host reads `limit + 1` rows to set `has_more`.
Unknown session 404; unknown `before` id or invalid limit 400. Omitting `before`
returns the newest page. Pages are not cached in the serving
view; the read is bounded. `SessionView` is unchanged.

**Frontend.** The store keeps older pages per selected session, separate from
`view` (which each refresh replaces), cleared on session change and on `reset`. Rendering merges `older` with every
snapshot seen since selection, deduplicated by message id, so the window sliding forward
as new messages arrive leaves no gap (the log is append-only). Older pages load when the
transcript is scrolled near the top, and through a visible "Load earlier messages"
button for keyboard use; it replaces the "Showing the latest N messages" note. Prepending
keeps the scroll position (scrollHeight delta). Tool results are matched to calls by
`tool_call_id` over the whole merged list, so page boundaries do not split them.

Limit: opening an archived chat still runs the existing `restore_chat` on first access;
this change leaves that unchanged.

## 2. Inline provider setup

Code: `crates/bubblepaw-runner/src/web_providers.rs` (router merged into the web host
behind the same Host/Origin/JSON boundary) and the frontend provider settings view (not part of this repository).

**Where it writes.** The file given by `--config` / `BUBBLEPAW_CONFIG` at launch,
otherwise `~/.bubblepaw/config.toml`; never the workspace layer. The host picks the path.
`toml_edit` keeps comments and unrelated tables; writes go to a temp file and are
renamed. Only `kind`, `base_url`, `key_env`, `auth_file`, `reasoning` and `models`
(`id`, `reasoning_replay`) are written, plus `[model] main`. Keys are referenced by
`key_env` only; no key value is accepted, stored, logged or returned. Clearing rules and
layer conflicts are in [config](config.md#web-provider-setup-writes).

**API.** Listed in the [web contract](web-contract.md#provider-setup). `GET` reports each
provider's source, `inherited_fields`, `key_present` (env var set, never its value),
`deletable` (defined in the target file), `active_main`, `launched_main` and
`reload_needed` (`restart_needed` only flags nonprovider settings requiring restart), plus the non-secret
`allowed_key_envs` list from the effective `[web] provider_key_envs` setting.

**Save and Test rules.** A base URL must use HTTPS unless its host is `localhost`,
`*.localhost`, an address in `127.0.0.0/8`, or `[::1]`. Credentials, queries and
fragments remain forbidden in base URLs. `key_env` must be a valid environment
variable identifier ending in `_API_KEY`, `_KEY` or `_TOKEN`, or be listed in
`[web] provider_key_envs` (default `[]`). The server reads that allowlist afresh on
each request. Startup and reload share these rules with Save, draft Test and saved settings being tested;
violations return 400 before any probe. The form mirrors the rules and preserves
values when the server rejects a request.

**Test, without generating.** 10 s timeout, no redirects, body capped at 2 MiB, at most
500 listed ids. Paths confirmed from official docs on 2026-10-13:

- api.openai.com (`responses`, `chat-completions`, `decisions`): `GET /v1/models`, bearer.
- api.anthropic.com (`anthropic-messages`): `GET {base}/models?limit=1000`, `x-api-key`
  and `anthropic-version`.
- openrouter.ai (`openrouter-decisions`): `GET /api/v1/key`, bearer.
- On these hosts 2xx means auth ok, 401/403 auth failed.
- opencode.ai: when the base path contains `/go/`, `GET /inference/go/v1/models`
  ([Go docs](https://opencode.ai/docs/go)); otherwise `GET /inference/v1/models`
  ([Zen docs](https://opencode.ai/docs/zen)). Send the provider kind's usual auth
  headers. 401/403 means auth failed; every other outcome leaves auth `unchecked`
  with "models list documented; auth not confirmed for this endpoint". A loaded
  list reports configured models as listed or missing.
- Other hosts (local OpenAI-compatible servers): `GET {base}/models`; 401/403
  is auth failed, anything else leaves auth `unchecked` ("endpoint not confirmed").
- `codex`: the local auth file must parse and hold a token, and the host must answer;
  the token is never sent; auth `unchecked` (no non-generating check).
- `scripted`: config check only.
- A missing key env var is a config error naming the variable; reachability is still checked.
- When a list loads, each configured model id is reported as listed or not.

**UI.** A right-side sheet opened from the topbar's `provider / model` button: provider
list, edit form (key env var name with set/not-set text), Test on the unsaved form with
text status rows, Save as the one apricot action, "Use as main model" per model, and
Delete or "Remove override" only for providers in the target file, with a confirm. After
a successful change a persistent notice says "Saved and applied"; the topbar shows
the active provider/model. Unapplied disk changes show a "Reload provider config"
action. Reload is disabled while the form has an unsaved draft. Failed save or reload
preserves active settings and form values.

**Known limits.**

- Provider setup applies live; host settings outside the provider scope require a
  restart. See [live configuration](config.md#live-provider-configuration-2026-10-14).
- A test sends the key from an allowed env var to the configured host using HTTPS
  (HTTP is allowed for loopback). Only local same-origin requests reach the setup
  endpoint. The suffix/allowlist rule restricts variable names; it does not establish
  trust in a custom HTTPS host.
- Frontend checks ran under bun on Linux (node is not installed there), with newer
  dependency versions than `package-lock.json` pins.
- Verification on 2026-10-13: Rust 1.97.1 formatting and strict workspace Clippy pass,
  as do 21 provider integration tests, 3 provider unit tests and TypeScript checking.
  All 117 frontend tests pass with `BUN_JSC_useJIT=false` and `--maxWorkers=1`.
  Update 2026-10-14: the default Bun 1.3.14 invocation crashed the provider-test worker
  (a JIT-dependent memory runaway); on Bun 1.4.3 the suite passes at default workers,
  124 tests in 7 files.
  This is local fixture acceptance; no live provider was called.

## Automatic completion turns (2026-10-14)

With agent or process services attached, the web host shares their completion inbox
and owns its wake driver and root harness registration. Idle completion turns use
the ordinary run and SSE event path, including `RunStarted`, streamed paragraphs
and `RunFinished`; generated `<harness …>` reminders and legacy `[host notice]` input are shown as “Host notice.” No
frontend feature changes are required. Stop suppresses wakes until accepted user
text; resume alone does not clear suppression. See [completion wake](child-sessions.md#completion-notices-and-idle-wake)
and [configuration](config.md#completion-wake). Local HTTP/SSE acceptance lives in
`bubblepaw-runner/tests/web_wake.rs`; no live model or visual acceptance is claimed.

## Decisions (2026-10-13)

1. Codex stays in the list. Its Test checks the local auth file (present, parses, has a
   token) and that the host is reachable; the UI shows auth as "unchecked (no
   non-generating check)".
2. The original save-only behavior is superseded on 2026-10-14: saves apply live,
   and explicit HTTP/stdio reload applies provider changes made on disk.
3. Setup writes the `--config` / `BUBBLEPAW_CONFIG` file when one was given, otherwise
   the user layer `~/.bubblepaw/config.toml`. Never the workspace config.
4. "Use as main model" writes the main model in the same file. The key is `[model] main`
   (the existing main-model setting); the endpoint keeps the name `PUT /api/roles/main`.
5. `messages_before` is the one core addition, tested on archived and row-compressed chats.
