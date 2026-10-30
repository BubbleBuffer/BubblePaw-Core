# Native account quota

Status: implemented 2026-10-20; live-check corrections updated 2026-10-21.
Codex reset parsing, SDK model attribution, rejection classification and credit
readings are implemented. Stored credit balances piggyback on changed quota rows;
the standalone CLI reports the latest stored account balance and its recording time.
The live findings below are supplied account evidence; this follow-up uses offline
fixtures and local transports. Account routing is outside this change.

## Accounts and readings

Each provider or extension entry names one login. Optional `account = "work"`
gives that login a stable identity; omission uses the entry name. Aliases that
use the same login must use the same account. Use different names for unrelated
logins: the shared meter key is `(account, window)` across the data directory.
Provider names remain on observations and request-usage rows for attribution.

```toml
[quota]
max_wait_seconds = 21600 # Six hours; 0 declines automatic quota waits.

[providers.codex]
kind = "codex"
account = "personal-codex"
auth_file = "~/.codex/auth.json"

[extensions.claude-code]
account = "work-claude"
command = "bun"
args = ["run", "src/main.ts"]
cwd = "../extensions/claude-code"
env = { CLAUDE_CONFIG_DIR = "/home/user/.claude-work" }
```

The wait setting and extension resources take effect at host startup. HTTP
provider account edits follow provider-generation reload; in-flight requests
keep their original account binding. Quota does not select or rotate accounts.

One quota host lives for each host process. Attached requests share its
in-memory readings, change tracking and pollers, which are built from the first
request's settings, so an account added later needs a host restart.

A reading contains provider, account, window, finite `used_percent` in 0–100,
nullable UTC `resets_at`, UTC `observed_at`, nullable `balance`, and source (`response_headers`,
`sdk_event`, `usage_api`, or `rejection`). Responses and typed failures carry
readings; messages do not. A run can return several independent window readings.

## Sources

- **Claude Code SDK:** successful and rejected rate-limit events cross the
  extension protocol, including status, utilization, limit type, reset and
  unified/overage window fields. SDK utilization is a fraction and becomes a
  percentage. The pinned SDK 0.3.293 types and its bundled Claude 2.1.293
  implementation establish the shape and fraction conversion; plain Node/Bun
  fixtures exercise forwarding. This is not a live SDK-event capture.
- **Claude subscription usage API:** the host reads
  `<CLAUDE_CONFIG_DIR or ~/.claude>/.credentials.json` afresh and requests
  `https://api.anthropic.com/api/oauth/usage` using the existing OAuth access
  token and `anthropic-beta: oauth-2025-04-20`. It parses `five_hour`, `seven_day`
  and `limits[]`; scoped limits use `kind:scope` window IDs. It never refreshes,
  writes or logs credentials. Explicit API-key extension instances and OAuth-token
  environment overrides are excluded from file-backed polling; SDK events remain available.
- **Codex subscription Responses:** `primary` and `secondary` windows come from
  `x-codex-<window>-used-percent` and `x-codex-<window>-reset-at` (epoch seconds).
  Those names and `window-minutes` are confirmed in the
  [Codex CLI parser](https://github.com/openai/codex/blob/main/codex-rs/codex-api/src/rate_limits.rs).
  `reset-after-seconds` is a tested compatibility fallback; current backend
  emission of that older spelling remains unverified. Without a valid absolute
  `reset-at`, a missing, empty or zero relative reset is unknown
  (`resets_at = null`), never the observation time.
  Only a positive relative value supplies a fallback reset; a valid absolute
  `reset-at` wins. Window duration does not
  change the stable window ID. No app-server dependency is used.
- **Codex credits:** the CLI's exact headers are
  `x-codex-credits-has-credits`, `x-codex-credits-unlimited`, and
  `x-codex-credits-balance`, verified in
  [the pinned parser, lines 219-222](https://github.com/openai/codex/blob/50379197779be0e5afcbddb014a34cd1fc08af53/codex-rs/codex-api/src/rate_limits.rs#L219-L222).
  Both boolean headers must parse (`true`/`false`, case-insensitive, or `1`/`0`).
  A `credits` reading has 0% used when credits are available or unlimited, 100%
  when both flags are false, and no reset. The raw balance string, or
  `unlimited`, stays fresh in host memory and is stored only alongside an otherwise
  necessary history row for that account. Balance changes alone never write rows.
  The `credits` window never justifies a quota wait.
- **Anthropic API keys:** `anthropic-ratelimit-{requests,tokens,input-tokens,output-tokens}`
  limit/remaining/reset headers produce `100 * (1 - remaining / limit)` and the
  supplied UTC reset. Invalid numbers are ignored. DeepSeek has no quota source
  in this version.
- **OpenCode Go (console usage API):** for each `opencode` or `opencode-*`
  route (the implicit `opencode` route too), the host reads a BubblePaw-owned
  console login from `<data dir>/auth/opencode-console-<account>.json` and
  requests `https://opencode.ai/console/api/go/status` with
  `authorization: Bearer <access>` and `x-org-id: <org>`. The `fiveHour`, `week`
  and `month` meters become `five_hour`, `week` and `month` windows:
  `used_percent = 100 * usedMicroCents / limitMicroCents` (capped at 100),
  `resets_at` from `resetsAt`, source `usage_api`. A meter without a positive
  limit is unknown. `https://opencode.ai/console/api/billing/status` adds the
  `credits` window, following the Codex credits design: 0% used when
  `useBalance` is true and `availableMicroCents` is positive, otherwise 100%, no
  reset, and `balanceMicroCents` as dollars truncated to cents (`10.32`) as the
  balance. Billing failure drops only the credits reading. No stored login means
  no source and no error. The Go API key is refused on console routes (401), so
  only a console login works.

The OAuth source is demand-driven: before using an account with stale readings
(five minutes), and after a 429. Calls share a per-account 60-second minimum;
the post-429 request does not bypass that floor. Failures back off from five to
thirty minutes. Expired credentials and HTTP 401/403 pause the source until the
credentials file changes. Polling reads the credential file to detect a change,
but performs no idle background requests. Local fake HTTP tests verify these
rules and unchanged credential bytes and modification timestamps.

The OpenCode source follows the same policy (on demand, five-minute staleness,
60-second floor including after a 429, five-to-thirty-minute backoff, no
background polling). Unlike the Claude credentials, the console login belongs
to BubblePaw, so BubblePaw refreshes it: when the access token expires within a
minute, or once after a 401 from the status route. The rotated token replaces
the file atomically (temporary file in the same directory, then rename), under a
per-account `.lock` file so two processes never spend one refresh token; a
login another process already rotated is adopted. A refresh answered with
`invalid_grant` means signed out: the source pauses until the file changes and
the host prints once (stderr and a tracing warning) that the user should run
`bubblepaw login opencode --account <account>`. A refreshed token that is still
refused pauses the same way. Fake-server tests in
`crates/bubblepaw-runner/tests/opencode_console.rs` cover parsing, rotation
written back, the `invalid_grant` pause, the 60-second floor, change-only
history and a 429 parking until the `five_hour` reset.

### OpenCode Console login

`bubblepaw login opencode [--account <name>]` runs the opencode CLI's device
flow against `https://opencode.ai/console/`: `POST auth/device/code`
(`client_id = opencode-cli`, `supports_org_scope = true`), then prints the
verification link (resolved against the console URL) and user code and polls
`POST auth/device/token` at the given interval, adding five seconds on
`slow_down`. `expired_token` (device codes expire within minutes) ends with a
request to rerun the command. The default account is the `[providers.opencode]`
entry's `account`, or `opencode`. The credential (access and refresh token,
expiry, organization) goes to the auth file above: user-only permissions on Unix
(0700 directory, 0600 file), never SQLite, never printed or logged; its `Debug`
form hides the tokens. `bubblepaw logout opencode [--account <name>]` deletes
it.

Caveats: these console routes are undocumented and were verified live on
2026-10-21 only; a shape change makes the meters unknown, not exhausted.
Cloudflare rejects default library user agents (error 1010), so every console
request sends an honest `user-agent: bubblepaw/<version>`; BubblePaw never
imitates a browser or the opencode CLI.

## Rejections and waits

Only unpublished requests that were rejected can be retried automatically.
Published output and unknown external effects retain the existing replay guards.

| Limit | Action |
|---|---|
| 429 without applicable exhausted-window evidence, or a short Retry-After/reset | Cancelable inline retry in the registry's retry policy (`provider/retry.rs`), with the 15-second short range |
| 429 naming an exhausted limit, applicable fallback exhaustion, or reset beyond the short range | Wait in the same run until reset, within `max_wait_seconds` |
| Spend cap, billing/credit failure, or provider non-retryable rejection | Fail immediately |
| Reset beyond the maximum | Fail with `quota exhausted until <time>`, account and window |
| Exhausted reading without a reset | Fail with an unknown-reset explanation |

Readings alone never block dispatch: a 100% window can coexist with successful
purchased-credit calls. Only an actual rate-limit rejection enters this policy.
The limit named by that rejection takes precedence: the SDK's `rejected` event
`rateLimitType`/`resetsAt`, or exhausted quota headers on the actual Codex or
Anthropic 429 response. Earlier successful responses and retry attempts remain
available to history and fallback selection, but cannot name the final rejection's
limit. Each observation batch applies chronologically; older completions cannot
replace newer account state.

If the rejection names none, use the earliest future reset among exhausted
windows applicable to the called model. Account-wide Claude `five_hour`,
`seven_day`, `weekly_all` and `session`, Codex `primary`/`secondary`, OpenCode
`five_hour`, `week` and `month`, and Anthropic header buckets apply to all models
on that account. Known model-scoped windows
apply only to the matching model family. Unknown kinds and the Codex `credits`
window and the OpenCode `credits` window do not apply to fallback classification. With no applicable evidence,
use the existing short backoff. An early wake can receive another 429 and be
classified again; the policy does not select a different model or login. Already
elapsed resets do not justify a new long wait. The process keeps the same
provider request future, run ID, counters and schema-retry state while waiting.
Normal cancellation and configured run timeouts still apply.

The host publishes `run_waiting_for_quota` with the marker and changes the stored
run status to `waiting_for_quota`. The marker holds only session ID, run ID,
account, window, resume time and reason. It is cleared immediately before retry
dispatch, or when the run finishes, fails or is cancelled. This boundary is
deliberate: a crash after retry dispatch cannot authorize another automatic
request just because an old quota marker existed.

## Restart behavior

On startup, the owning web/stdio host schedules retained markers at their reset
times; elapsed waits resume promptly. The existing resume path admits a **new
run**, retaining canonical history and the selected model. Its round/tool budgets
and retry counters restart as for an ordinary resumed interrupted run. There is
no continuation checkpoint. The marker is checked and consumed atomically with
admission, so a cancellation or replacement cannot race into an unwanted resume.

Child recovery restores the original profile, workspace and saved restrictions,
and applies current host restrictions. If the original dynamic worker profile or
workspace is unavailable, recovery fails clearly instead of granting another
profile's capabilities. An unresolved external effect discards automatic-recovery
eligibility. Ordinary interrupted runs without quota markers never auto-resume.

One-shot `run`, `resume`, `script` and delegation hosts list pending waits and
resume due ones before their requested operation. They do not stay alive solely
to service future markers; the next owning host performs that work. No new daemon
is installed. Read-only quota/usage/report commands do not start model work.
Graceful cancellation, including host shutdown of an active run, clears its wait;
markers survive a process interruption while the quota wait itself is pending.
If storage cannot clear the marker, cancellation returns `false` and emits a
`quota_cancel_failed` observation when diagnostics are enabled. The caller must
retry cancellation after resolving the storage error; an in-memory cancellation
alone cannot prevent recovery after a restart.

## Storage and CLI

`catalog.db` contains authoritative, data-directory-wide `quota_history`. A row
is appended only when the account/window percentage or reset changes. Provider,
source, balance and observation-time changes alone add no write transaction or WAL bytes.
The host keeps fresh observation ages in memory. Restart and standalone CLI
reports use the last durable change's timestamp and source. Quota history survives
chat deletion, archive, serving wipe and catalog projection rebuild.

Wait markers and waiting run records live in the authoritative chat file. They
block archival until resolved. Neither readings nor markers belong in disposable
`serving.db`, and no full request or prompt is copied into a marker.

```sh
bubblepaw quota --data-dir DIR
bubblepaw quota export --data-dir DIR
bubblepaw quota export --data-dir DIR --since 2026-10-20T00:00:00Z
```

The table shows provider, account, window, percentage, reset, recorded age and
source, with a `CREDIT BALANCE` column. Export is chronological JSONL; `--since`
accepts an inclusive RFC 3339 instant, including offsets. Empty exports print
nothing. Every line has nine
fields, including nullable reset and balance. The additive `balance` field keeps
the schema identifier `bubblepaw.quota.v1`:

```json
{"schema":"bubblepaw.quota.v1","observed_at":"2026-10-20T12:00:00Z","provider":"claude-code","account":"work-claude","window":"five_hour","used_percent":72.5,"resets_at":"2026-10-20T15:00:00Z","source":"usage_api","balance":null}
```

Both commands read the catalog without provider setup, credential access or
polling. Per-request tokens remain separately documented in [usage tracking](usage-tracking.md).

### Stored balance decision (2026-10-21)

The catalog is created at version 2 with a nullable `balance` column
on `quota_history`. A row still writes only when an
account/window percentage or reset changes. Whenever such a row writes, for any
window, it carries the account's latest known balance. This includes a `credits`
row when credit availability changes. Ten changed balances with unchanged quota
percentages and resets still produce only the initial history row. A later
secondary-window change carries the latest balance, including credit metadata
from the same response even when its credits reading follows the secondary one.
Batches containing several attempts are applied in observation-time groups;
a later group does not supply the balance for an earlier group. Missing balance
metadata carries forward the last stored account balance on the required row.

`QuotaHost.latest_balance(account)` exposes the fresher in-memory value and its
observation time. The standalone
`bubblepaw quota` command shows the most recent stored balance across the account's
windows as `balance 12.34 as of <time>`. Selection uses the last stored balance
row, even when a late completion has an older observation timestamp. The time is
the history row's `observed_at`,
when that known balance was recorded; it does not imply a fresh balance query.
The standalone balance can lag until another percentage or reset change writes a
row. Restart likewise recovers the last stored value. No cache file is used.
Missing balances display as unknown for credits; older catalogs remain readable
without a migration by these read-only commands. JSONL export includes each row's
stored balance, or null, without substituting a newer account balance.

## Live-check findings (2026-10-21)

The supplied real-account check found Codex `secondary.resets_at` equal to its
nanosecond `observed_at`. This is consistent with the relative fallback accepting
zero; the captured reading does not prove which header spelling the backend sent.
Missing, empty and zero relative-reset cases now have explicit parser fixtures.

Codex `primary` reported 100% while calls succeeded on purchased credits. This
reading is valid and must not become a dispatch gate. The Claude usage API also
returned `weekly_scoped:Fable` and the unknown kind `iguana_necktie` at 100%, with
the latter resetting on 2026-11-05. Those observations motivated model-aware
fallback selection and ignoring unknown window kinds; they do not identify a
particular request rejection. The former policy selected the latest reset across
all exhausted account windows, which could incorrectly exceed the wait maximum.

Claude Code calls configured as `haiku` retained the alias in request usage and
therefore had null cost. The corrected reported-model accounting is documented in
[usage tracking](usage-tracking.md).

## Follow-up verification (2026-10-21)

All Rust commands ran on Linux in the isolated `quota-fixes`
worktree, on the Linux host's persistent work and target directories. No Rust build or
test ran on the PC. The full workspace test suite and live provider acceptance
were not run. The final focused tests passed:

| Target | Passed |
|---|---:|
| Core integration: `quota` | 13 |
| Runner integration: `provider_continuation` | 22 |
| Runner integration: `provider_ext` | 31 |
| Runner integration: `provider_quota` | 7 |
| Runner integration: `provider_retry` | 8 |
| Runner integration: `quota_cli` | 3 |
| Runner integration: `quota_rejection_evidence` | 2 |
| Runner integration: `quota_scheduling` | 16 |
| Runner library filter: `quota_sources::tests` | 18 |
| Claude Code extension: `npm test` on the PC | 164 |

The single ignored `quota_scheduling` subprocess helper is invoked by its parent
restart test. `npm run typecheck`, remote `cargo fmt --check`, and remote
`cargo clippy --all-targets -- -D warnings` passed. These targets account for 120
distinct passing Rust tests.

Fixtures cover current named-limit precedence, earliest applicable fallback,
other-model scopes, unknown kinds, no-evidence backoff, successful dispatch at
100%, unchanged max-wait/spend-cap behavior and publication guards. Retry and
continuation observations remain in history without naming a later rejection;
same-window batches apply in chronological order. The local Codex transport
fixture performs ten successful responses with different balances and proves
one history row plus the latest balance in memory. These were the initial checks before the stored-balance decision; the follow-up
below supersedes their balance-storage and export expectations.

### Stored-balance follow-up verification (2026-10-21)

After the decision to piggyback balances on existing writes, these focused Rust
integration targets passed on Linux:

| Target | Passed |
|---|---:|
| Core: `quota` | 15 |
| Core: `schema_compatibility` | 2 |
| Core: `storage_layout` | 11 |
| Runner: `provider_quota` | 10 |
| Runner: `quota_cli` | 5 |
| Runner: `quota_scheduling` | 16 |

These are 59 distinct passing tests. The single ignored scheduling helper is
invoked by its parent subprocess test. Remote `cargo fmt --check` and
`cargo clippy --all-targets -- -D warnings` passed after the final code change.
Formatting was applied on Linux and the formatted sources copied back into the
isolated worktree. No Rust command ran on the PC; no live provider calls or full
workspace test suite were run. The unchanged TypeScript extension was not retested.

Ten changing balances produce one history row and one catalog transaction.
A later secondary percentage change and a credits-availability change both
carry the latest balance. Additional fixtures cover stored-balance fallback,
account isolation, same-response ordering, multi-attempt timestamps and stale
balance arrivals. Standalone subprocess tests show the lagged stored value,
then the newer piggybacked value; they also select the last stored account balance
after a late completion. Export checks require nine fields, nullable balance and
the unchanged `bubblepaw.quota.v1` schema identifier.

## Verification (2026-10-20)

All Rust checks ran on Linux, using its ordinary persistent
work and target directories. No Cargo command ran on the PC and no full workspace
test suite was run. The focused checks passed:

| Targets or filter | Passed |
|---|---:|
| Core: `quota`, `request_usage`, `schema_compatibility`, `storage_batching` | 36 |
| Runner: `provider_accounts`, `provider_continuation`, `provider_ext`, `provider_messages`, `provider_quota`, `provider_retry`, `quota_cli`, `quota_scheduling`, `tracing`, `usage_cli` | 110 |
| Runner library: `quota` filter, including fake HTTP polling and child recovery | 21 |
| Runner library: `config::tests`, `usage_report::tests`, `provider::live_provider::binding_tests` | 54 / 10 / 4 |
| Claude Code extension: `npm test` (Bun's plain test runner) | 145 |

Library filters overlap, so these rows are not a distinct-test total. Two ignored
subprocess fixture entries are invoked by their parent tests; three paid Messages
tests remain ignored. `npm run typecheck`, `cargo fmt --check`, and
`cargo clippy --all-targets -- -D warnings` passed. Strict Clippy also passed for
the runner with `--features web --all-targets`.

The restart fixture exits a real process while its marker is pending, reopens
the same data directory in a new process, and checks a different run ID completes
at reset. It covers persistent scheduling, due one-shot recovery and explicit
resume without double admission. Further fixtures cover cancellation before
process exit, failed durable cancellation acknowledgement, atomic stale-marker
rejection, unknown effects and zero-retention housekeeping.

| Fixture | Chat / serving / catalog commits |
|---|---:|
| Ordinary final turn, baseline and quota-enabled | 2 / 1 / 0 |
| Single Allow native-tool turn, baseline and quota-enabled | 4 / 1 / 0 |
| Hosted normal turn with an unchanged reading | 2 / 1 / 0 |
| Repeating an identical observation 25 times | 0 / 0 / 0 |
| First changed reading | 0 / 0 / 1 |

Identical observations add zero WAL bytes. The ordinary fixture keeps aggregate
database bytes at 12,288 and reports WAL 922,976 → 1,116,616; the native-tool
fixture reports 922,976 → 1,219,616. The hosted unchanged-meter fixture reports
database bytes 12,288 → 12,288 and WAL 1,128,976 → 1,314,376. These are whole-fixture
figures across the chat/catalog/serving files before checkpointing, including
ordinary message and operational writes; they are not per-row sizes or latency
measurements. The existing bounded-WAL and checkpoint fixtures pass.

No live provider request, billing reconciliation or physical power-loss test was
performed. Codex's `reset-after-seconds` emission remains unverified; SDK field
shape and fractional utilization are source-verified and fixture-tested, without
a live event capture.
