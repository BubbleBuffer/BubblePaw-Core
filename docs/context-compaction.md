# Context compaction facts

Status: implemented 2026-10-27.

LCM gathers memory independently of applying it. Gathering writes episodes and
closes subject chapters; applying replaces the frozen serving prefix. Original
messages stay available. The [LCM contract](lcm-contract.md) defines those
operations; this page describes their recorded facts.

## Events and reasons

The chat database stores `ContextEvent` JSON in `context_events`, with stable
insertion-order reads from `Store::context_events(session_id)`. Events contain
IDs, creation time, covered first/last canonical message IDs, covered episode
IDs, an optional summary ID, a trigger and a short explanation.

| Event kind | Trigger | When recorded |
|---|---|---|
| `episode_written` | `source_size` | Ordinary gathering reaches the source target or maximum at a complete exchange boundary. |
| `episode_written` | `subject_change` | The decider selects a subject boundary. |
| `episode_written` | `pressure` | Admission requests an ordinary pressure-gather step. |
| `episode_written` | `emergency` | Admission forces a short complete episode to make room. |
| `episode_written` | `manual` | Explicit compaction gathers older complete history. |
| `chapter_closed` | `subject_change` | A split closes the previous chapter; the event covers that chapter's members. |
| `chapter_closed` | `source_size` | The configured chapter summary-token rollover closes the chapter. |
| `applied` | `pressure` | Automatic preparation exceeds an input admission cap and publishes new coverage, from either `Request` or `Pressure`. |
| `applied` | `manual` | `Compact` publishes a changed view. |

Other chapter closes inherit the gathering batch's trigger. `trigger_text` is
at most 160 Unicode characters and contains numeric metadata, such as source
tokens and a target or a canonical message sequence number. Manual events state
that the user asked to compact; focus text, message contents and summary contents are
not copied into explanations. Byte-budget pressure names the byte limit rather
than claiming that the token cap was exceeded.

Every event's `tokens_before` measures covered original source tokens when that
count is available. It is null for coverage containing inherited imported
summaries or legacy checkpoints without stored source counts. Counting never
loads cold originals solely to fill this field; the event and its full range
are retained. `tokens_after` measures the replacement summary text. These differ from
the whole-request `input_before` and `input_after`, which are present only for
apply events. The input fields use the same prepared-request estimate as the
admission check, including system instructions and tools. A `Pressure` retry
reserves missing harness overhead by lowering the admission cap; that reservation
is not added a second time to the recorded estimates. `input_estimated` is true.
When calibration from earlier provider usage pushes admission above the cap,
the explanation labels the calibrated count; the input fields still retain the
canonical/provider-wire estimate used to evaluate the candidate view.

An ordinary request that reuses the saved view records no apply event. Failed
gathers and rejected apply candidates publish no successful compaction fact.

## Applied view blocks

`ContextView.blocks` records the token partition when the view is applied, in
model order. `blocks_estimated` is true. The kinds are `opening`, `memory_index`,
`anchors`, `chapter_summary`, `episode_summary` and `raw_suffix`. Summary blocks
carry the chapter or episode ID and covered canonical message IDs. Original
anchors and the raw suffix retain their source ranges.

Blocks count the rendered view, including summary headings and message framing,
without duplicating combined summary-message wrappers. The opening includes
system instructions, tools and the initial request. Any positive difference
between the provider's wire estimate and the canonical estimate is accounted for
as opening request framing. The sum matches the view's input estimate within
the fixed request overhead.

These are apply-time facts. Later requests can append raw messages without
rewriting the frozen view or its block snapshot. Old view JSON without block
metadata loads with an empty block list.

## Message token reads

`bubblepaw_sdk::store::message_tokens(&store, session_id, first_seq..=last_seq)`
returns chronological
`MessageTokens` rows with `message_id`, `tokens` and `source`:

- `reported`: assistant output tokens from the successful provider request that
  produced the message. When a response joins successful continuation requests,
  their linked output counts are summed. Failed attempts are not attributed to
  the assistant message.
- `estimated`: an O200k canonical-message estimate computed on read. This covers
  user/tool messages, old assistant rows and providers without usage. Estimated
  counts are not persisted. An explicitly reported zero remains reported.

Core's `Store::reported_message_tokens(session_id, first_seq..=last_seq)` returns
only reported counts, with missing counts represented as null. Core contains no
token estimator. The SDK fills missing counts using the runner's existing
canonical message estimator, including
Responses replay framing. Opaque encrypted reasoning cannot be estimated from
its ciphertext. These estimates are not provider billing measurements.

## Storage and transaction budget

Gather facts share the existing `save_episode_batch` chat transaction. Applied
facts share the existing view-validation chat transaction; central layouts then
save the disposable view in `serving.db`. There is no new gather transaction,
no write for a view reuse, and no separate transaction for message token links.
The nullable `request_usage.message_seq` link is set with the assistant's
existing append/dispatch/completion transaction.

Each fact also produces a `context_compaction` session event containing its ID,
kind and trigger. Single-file stores journal it in the same transaction; central
stores buffer it with the existing serving/catalog flush, without a separate
commit. Clients can detect the stdio `context` feature, read the Context data,
and follow notifications through the ordinary event stream. The exact methods
and payloads are in [session API](session-api.md#context-reads-and-compaction-notifications-2026-10-27).

Compaction events and reported-message links survive a serving-store wipe.
See [storage model](storage-model.md) for database placement and the
[SDK](sdk.md#context-facts) for supported Rust reads.

## Verification

Scripted core, runner and SDK tests cover schema migration, atomic event writes,
trigger mappings, Request-path pressure, forced gathering, manual compaction,
view reuse and block totals. Poisoned inherited-source tests verify that apply
facts do not decode cold originals. Message-attribution tests distinguish omitted
counts from explicit zero and retain discarded fallback billing without linking
it to the assistant message. These tests do not establish live-model quality.
