# Native usage tracking

Status: built 2026-10-20; focused scripted and local-transport verification on
Linux. This is request accounting from provider-reported tokens, not a
subscription-quota meter or a live-provider billing reconciliation.

## Durable records

Every dispatched model request has a small row in `request_usage` in the owning
chat SQLite file (schema 9). Main turns, physical output continuations, summaries,
chapter/classical summaries, repeated decider samples, retries and fallback
routes keep separate request UUIDs. Failed calls retain any reported billable
usage; missing counts are zero and unknown reasoning is null. A provider future
interrupted before it reports usage cannot reveal unreported tokens.

Messages streams retain usage reported by `message_start` and subsequent usage
deltas when the stream fails, including input, cache reads and cache creation.
Native decision adapters pass reported output and cache counts to model-role
consumers as well as the request ledger. Decision and chat parsing share the
DeepSeek `prompt_cache_hit_tokens` mapping.

Each row contains request ID, UTC timestamp, session ID, optional run ID, actual
provider/account/model, existing request kind, token counts and a failure flag. The
existing kinds are `main`, `summary_fork` (all summarization roles) and `decider`
(including automatic approval decisions). Background and standalone auxiliary
operations may have no owning run. The failure flag describes the physical
provider request/parsing outcome; later host validation can reject an otherwise
successful response. No request/response bodies or prices enter this table.

Claude Code usage rows record the full model ID the Agent SDK reports rather than
the configured alias: the model on assistant and stream messages (kept even when
a message reports no usage), or a `modelUsage` key of the result when messages
carry none (the largest output share when one step names several). A step or
error that reports `reported_model` names its row with it. The
configured name (`haiku`, `opus[1m]`, ...) stays the request and replay identity,
and is the fallback only when the SDK reports no model. Failed calls keep the usage
and reported model the SDK returned with the error.

Input tokens include both cache reads and cache creation. Reasoning tokens are
already included in output and must not be charged twice. Custom provider
implementations without physical instrumentation contribute one row per provider
invocation. Direct standalone registry calls and replay judges also persist rows
when an authoritative store and session are available.

Rows survive serving wipes, operational retention, summary replacement, child
cleanup and archives. Forking history does not copy billed calls. Deleting a root
chat removes its file and usage. Existing schema-7 histories have no historical
request rows to backfill; aggregate usage cannot reconstruct per-request facts.
Schema-8 request rows acquire an account equal to their recorded provider during
migration; current configuration cannot identify an older request's account.

Provider and extension entries accept an optional `account` string. Omission uses
the entry's name; explicitly assigning the same label to several routes shares
their quota identity while retaining each physical provider route in usage.
For example, `extensions.claude.account = "personal"` and
`extensions.claude-summary.account = "personal"` share a meter when both use
the same Claude login. Labels are
1–256 bytes and cannot have surrounding whitespace or control characters. They are
identifiers, so credentials and request content never belong in them. Account
attribution follows the actual route on fallbacks, including summary forks,
auxiliary retries and decision calls. Provider account edits apply with live
provider reload; extension configuration retains its existing restart boundary.

## Report and export

```sh
bubblepaw usage --data-dir DIR
bubblepaw usage --data-dir DIR --since 2026-10-20T00:00:00Z --session SESSION_ID
bubblepaw usage export --data-dir DIR --since 2026-10-20T00:00:00Z
```

Data-directory selection follows the runner's existing `--data-dir`, configuration
and workspace defaults. Reports open the authoritative files read-only, including
packed archives, without registering providers, reading credentials, applying
retention or restoring chats. `--since` is an inclusive RFC 3339 instant; offsets
are accepted. `--session` filters the report by the original session ID, including
retained billing for a removed child.

The default report is JSON: overall totals and groups by session, model, provider
and UTC day. Each group includes request/failure counts, total input, cache read,
cache creation, output, optional reasoning, priced subtotal and unpriced count.
A group with any unpriced requests has null total cost; its priced subtotal is
identified separately.

Export writes one JSON object per request to stdout, in timestamp order (UUID
breaks equal-time ties), and exits successfully without output when no rows match.
Every line has these fields; null-valued fields remain present:

```json
{"schema":"bubblepaw.usage.v1","request_id":"<uuid>","timestamp":"2026-10-20T12:34:56.789Z","session_id":"<id>","run_id":null,"provider":"opencode","account":"opencode","model":"deepseek-v4.1-flash","kind":"main","input_tokens":1000,"cache_read_tokens":400,"cache_creation_tokens":0,"output_tokens":100,"reasoning_tokens":null,"failed":false,"cost_usd":0.0001512,"price_date":"2026-10-19"}
```

`input_tokens`, `cache_read_tokens`, `cache_creation_tokens` and `output_tokens`
are nonnegative integers. `reasoning_tokens` is a nonnegative integer or null;
`run_id`, `cost_usd` and `price_date` are also nullable. `failed` is a boolean.
`account` is a string immediately after `provider`. It is an additive field in
`bubblepaw.usage.v1`; consumers should select that schema and tolerate additive
fields. Historical exports produced before account tracking contain 16 fields;
current exports contain 17.

## One dated price table

`crates/bubblepaw-runner/src/prices.json` is the shared dated source for delegation
ledger prices, native reports/exports and the existing retained session projection.
The current table date is 2026-10-19. Usage rows store no cost; reports compute it
at read time, selecting the request's time period and long-prompt rule. Changing
the table can therefore change reported historical cost. Delegation ledger
snapshots retain their existing write-time cost contract.

Only exact known model IDs (after a provider prefix) select rates. An unknown
model, unpublished used token class or inconsistent cache/input counts produces
null cost and null export price date. There is no substring or family-price guess.
A reported dated snapshot ID that is not listed in the table therefore prices as
null until the table names it; a bare alias such as `haiku` never prices.
These are list-price calculations, including for subscription-backed model IDs;
they do not represent actual account invoices or subscription-quota consumption.

## Transactions and verification

Normal main rows share the assistant/step transaction. The native single-Allow-tool
fixture remains 4 chat + 1 serving + 0 catalog commits, matching main; the ordinary
final-turn fixture uses 2 chat + 1 serving + 0 catalog. Usage adds zero commits.
Run totals in `runs.data` and the `RunFinished` event retain their previous shape.
Automatic-approval decider rows share the existing approval transaction. The
existing `UsageContext` latest-prompt observation still adds its preexisting one
transaction per response; the new request ledger adds none.

Auxiliary rows share the final derived-memory/checkpoint transaction where one
exists. Small standalone transactions occur for direct role/registry calls and
replay judges without a canonical result write, terminal failures, completed work
before another separate model operation, or a dropped capture. Empty flushes
write nothing. An unknown interrupted external effect is never replayed.

Focused acceptance covers schema-7 monolithic and split-chat migration, canonical
row preservation, atomic rollback, exact physical continuation/failure usage,
summary/decider attribution, cleanup/archive retention, unchanged transaction
counts, exact JSONL fields, inclusive timestamp filtering and shared pricing.
The check environment uses a temporary RAM filesystem on Linux because its
persistent filesystem was full; transaction counts and byte sizes are meaningful,
while this is not disk-latency or power-loss acceptance. No live model calls are
needed for these checks. See [storage model](storage-model.md#durable-request-usage-2026-10-20)
and [session API](session-api.md#context-fill-and-retained-usage-updated-2026-10-20).


### Model attribution checks (2026-10-21)

On the PC, `npm test` in `extensions/claude-code` passed all 164 tests, and
`npm run typecheck` passed. On Linux, runner `provider_ext` passed
all 31 tests, including alias/full-ID pricing, configured-name fallback and
failed-call usage attribution. The three added Rust cases are
`reported_full_model_id_names_the_usage_row_and_prices_it`,
`configured_alias_is_the_fallback_when_no_model_is_reported` and
`failed_call_keeps_reported_usage_and_the_reported_model`.

The SDK fixtures cover persistent tool/final steps, one-off summaries,
`result.modelUsage`, errors, and model-bearing messages without usage objects.
These are scripted SDK and local extension transport checks; they do not
requalify live Claude Code billing or rewrite older alias-bearing usage rows.
Remote `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` also
pass; see [quota verification](quota.md#follow-up-verification-2026-10-21) for
the complete focused target list.

### Check results (2026-10-20)

All Cargo commands ran on Linux, with two build jobs,
no incremental compilation and no sccache, using the task's temporary RAM work
and target directories. No Cargo build or test ran on the PC. The full workspace
test suite was not run.

| Focused targets | Passed |
|---|---:|
| Core integration: `context_chapters`, `context_checkpoint`, `context_episodes`, `request_usage`, `schema_compatibility`, `storage_batching`, `storage_cold`, `storage_layout`, `store_storage` | 84 |
| Core integration: `usage_compatibility` | 6 |
| Core library: `approvals::tests::auto_` and `approvals::tests::cancellation_during_auto_call` filters | 7 |
| Runner integration: `aux_usage`, `decision_providers`, `delegate`, `episode_models`, `provider_continuation`, `usage`, `usage_cli` | 94 |
| Runner integration: `provider_ext`, `provider_retry` | 36 |
| Runner library: `pricing::`, `usage_report::`, `delegate::` and `parallel_retry_captures_each_billable_attempt_without_leaf_duplicates` filters | 28 |
| **Distinct current-branch tests** | **255** |

One subprocess crash-fixture entry is intentionally marked ignored; its parent
test invokes it. `cargo fmt --check` and
`cargo clippy --all-targets -- -D warnings` passed. The JSONL tests parse exactly
16 schema fields, cover nullable fields and UTC normalization, and verify empty
output, inclusive `--since`, offset-equivalent instants and report grouping.

An isolated source archive of `main` at `cdf9c40` independently passed the
existing native-tool transaction test and a small ordinary-final-turn count
probe. Counts before/after this change are:

| Fixture | Main chat / serving / catalog | Usage tracking chat / serving / catalog |
|---|---:|---:|
| Ordinary final turn | 2 / 1 / 0 | 2 / 1 / 0 |
| Single Allow native-tool turn | 4 / 1 / 0 | 4 / 1 / 0 |
| Automatic approval insert | 1 existing commit | 1 commit including decider usage |

The one-request ordinary fixture reports 12,288 aggregate database bytes before
and after, with aggregate WAL bytes 877,656 -> 1,071,296. The two-request native
tool fixture also reports 12,288 database bytes, with WAL 877,656 -> 1,174,296.
These are whole-fixture figures across the chat/catalog/serving files before
checkpointing, including ordinary messages and operational writes; they are not
per-row sizes or disk-latency measurements. Existing bounded-WAL and explicit
checkpoint tests pass.

No live provider calls, invoice reconciliation, packaged release or physical
power-loss test was performed. The persistent work filesystem remained full;
no unrelated source, database or build cache was deleted.

### Provider usage parsing fixes (2026-10-20)

The follow-up focused run on Linux passed 91 tests across `provider_messages`
(18), `decision_providers` (22), `provider_chat` (19), `provider_continuation` (22),
`aux_usage` (4) and `provider_usage_parsing` (6). Three paid Messages live tests
remain ignored. The new cases cover partial Messages streams ending in error
events or premature EOF, usage deltas before failure, all native decision kinds,
chat-backed DeepSeek decisions, malformed-answer usage, explicit zero cache
counts, repeated model-role totals and persisted per-request counts.
`cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` passed through
Linux as well. No Cargo commands ran on the PC.

These checks used the ordinary persistent Linux work and target directories;
the earlier RAM-filesystem limitation above applies to the original acceptance
run. No live billing reconciliation was performed.
