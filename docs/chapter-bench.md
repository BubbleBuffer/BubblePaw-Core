# Chapter-selection benchmark

Status: implemented 2026-10-23; scripted, local-transport and live-model results
are separate acceptance levels. A dry run checks the benchmark machinery and
does not establish model quality.

The benchmark compares production chapter deciders on identical recorded
chats. Every arm walks the same frozen episode boundaries, keeps its own chapter
state, and scores against independently supplied labels. It does not replay any
archived tool, continue the original task, or use historical decision inputs.

## Chapter prompt comparison

The separate `bubblepaw-episode-prompt-bench` binary also compares chapter
prompts v1 and v2 over the same frozen episode inputs, without regenerating
episodes. Build with the `bench` feature and use the checked-in synthetic
fixtures for a scripted check:

```text
bubblepaw-episode-prompt-bench --chapter-fixtures crates/bubblepaw-runner/tests/fixtures/chapter-prompt-bench.json --out <output-dir> --dry-run
```

The fixture JSON supplies episode memories with their original `EPISODE N
[source ...]` headers, typed requests, delegated instructions and optional
earlier chapters. The report contains the shared input fingerprint, each arm's
output, usage and elapsed time. For v2 it also keeps the raw text after the prose
(`metadata_text`, the Decisions and Sections blocks as the model wrote them) and
the parser error (`metadata_error`, null when the blocks parsed), so a fallback
can be explained without another call; both are null for v1. The runner's
storage path is unchanged and still drops invalid blocks. `--dry-run` uses an in-process scripted provider
and does not load credentials; it checks the machinery, not model quality.
Omitting it invokes `opencode/deepseek-v4.1-flash` through the configured provider
registry and can incur charges; only run that comparison with the owner's
authorization. `--max-cost-usd` bounds calls
between invocations, so one call may exceed the remaining allowance.

## Invocation

Build the runner with the `bench` feature. Supply each chat and arm explicitly:

```text
bubblepaw chapter-bench \
  --chat path/to/canonical-corpus.json \
  --chat path/to/brain/chats/chat-a \
  --gold path/to/brain/labels-lead.json \
  --out path/to/benchmark-output \
  --arm jev=decider:opencode-decisions/jev-1.13-free \
  --arm flash-decider=decider:llm/opencode/deepseek-v4.1-flash \
  --dry-run --max-cost-usd 0.50
```

`--chat` and `--arm` repeat. Arms use `name=decider:model`;
names and model routes are explicit so the saved result identifies the comparison.
`--dry-run` uses scripted responses without credentials or model requests. Omit it
only for an authorized live evaluation. `--refresh-episodes` creates a fresh
episode stage, retaining earlier stages for inspection. `--max-cost-usd` defaults
to USD 0.50 and checks cumulative recorded spend before each provider invocation. One call
can exceed the remaining allowance; later calls then stop. An interrupted attempt
or unknown cost blocks further calls, including after restarting the command.

The Windows workflow uploads `bubblepaw-windows-bench` in addition to the ordinary
artifact. With its executable in the worktree root and `OPENCODE_API_KEY` already
set, this PowerShell command uses the checked-in OpenCode provider configuration:

```powershell
$brain = '<corpus>'
.\bubblepaw.exe --config examples/worker-opencode-go.toml chapter-bench `
  --chat "$brain\chats\chat-a" --chat "$brain\chats\chat-b" `
  --chat "$brain\chats\chat-c" --chat "$brain\chats\chat-d" `
  --chat "$brain\chats\chat-e" --gold "$brain\labels-lead.json" `
  --out .run/chapter-bench-live `
  --arm jev=decider:opencode-decisions/jev-1.13-free `
  --arm flash-decider=decider:llm/opencode/deepseek-v4.1-flash `
  --max-cost-usd 0.50
```

Raise the cap explicitly to continue after a cost stop. Keep the output directory,
including its episode stage stores and ledger; completed episodes are reused.
Use a different output directory for a dry run. The frozen files are
`<out>/episodes/<chat>.json`; `--refresh-episodes` is the explicit regeneration
switch. A frozen file with missing stage metadata fails instead of spending on
an implicit replacement.

To retry an interrupted episode with raw provider diagnostics, use one `--chat`,
the same configuration and `--out`, and **omit** `--refresh-episodes`. Completed
summaries remain committed; generation starts at the first uncovered episode.
`--refresh-episodes` instead regenerates all episodes of that selected chat.
For the saved chat-d stage covered through `r4280`, the next request covers
`r4282`–`r4595`; with an updated Windows `bench` artifact:

```powershell
$brain = '<corpus>'
$env:BUBBLEPAW_PROVIDER_DUMP_DIR = '<out>\provider-dumps'
try {
  .\bubblepaw.exe --config examples/worker-opencode-go.toml chapter-bench `
    --chat "$brain\chats\chat-d" --gold "$brain\labels-lead.json" `
    --out <out> `
    --arm jev=decider:opencode-decisions/jev-1.13-free `
    --max-cost-usd 3.00
} finally {
  Remove-Item Env:BUBBLEPAW_PROVIDER_DUMP_DIR
}
```

The cap is cumulative for that output directory. This resumes the failed episode
without repeating earlier summaries; after success it continues later episodes
and the selected arm. There is no single-episode stop flag. An unresolved ledger
attempt still blocks dispatch; the dump option does not bypass the ledger.

## Read-only inputs and scoring

Canonical inputs use the existing `ReplayCorpus` JSON shape. A supplied
`brain/chats/<name>` directory resolves to `brain/fixtures/<name>.json`, which
contains the complete pinned normalized records. Its `sheet.json` contains only
typed-user excerpts and is not a replay source. Direct pinned fixture JSON is
also accepted. The loader does not follow fixture `source_path`, regenerate source
snapshots, or load archived summaries, questions, decisions or per-chat labels.

Conversion preserves source record IDs and zero-based `record_index` and
`typed_user_index` metadata. It follows the retired Rust-replay exporter:
instruction records become system messages; agent observations become assistant
messages; tool arguments parse as JSON when possible and otherwise remain text;
recorded exit status and error flags remain errors. Known injected wrappers,
child tasks and `<turn_aborted>` notices do not count as typed input. Deterministic
credential-pattern screening matches the historical exporter; arbitrary private
archives still belong in private output directories. The shared replay archive
normalizer adds explicit unknown-outcome markers for calls with no recorded
result, without executing tools or inferring their outcome.

`--gold` supplies `labels-lead.json` with `chats.<chat_id>.chapters` and optional
`ambiguous` records. The loader validates the chat ID, typed record IDs, unique
chapter IDs, source order and the initial chapter. The first chapter must begin at
the first typed message; its initial chat start is excluded from scored gold.
Ambiguous records must also name typed source IDs. Embedded canonical labels are
replaced by this supplied file. Exact and tolerant scores use the replay
`boundary_score` implementation, with maximum one-to-one matching; tolerant
matching allows typed distance at most one or a predicted ambiguous record.

Candidates begin between episodes; the initial episode does not create a scored
candidate. An accepted boundary maps from the candidate episode's first source ID
to the first typed message at or after it. A candidate with no later typed message
maps to no scored boundary. The call ledger identifies the candidate source
boundary; report probes retain each candidate, its mapped ID and the selection.
Scored predictions use the mapped typed IDs. Several candidates mapping
to the same typed ID produce one scored prediction.

## Shared episodes and independent walks

Episode preparation runs the current production gather-and-summary path once,
with `opencode/deepseek-v4.1-flash` as the summary model and chapter decisions
disabled, then freezes the resulting episodes. Source and
configuration fingerprints pin resumable stages. Successful committed summaries
survive interruption. The final uncovered tail stays explicit rather than being
silently summarized by a different rule. Changing source, mode or configuration
requires a new episode stage. Episode-generation calls and costs are reported
separately from each arm's chapter decisions.

Decider arms use the production chapter question and the arm's own open-chapter
state. Every arm starts from its own state and cannot read another arm's decisions.
Only episode preparation resumes committed work. Every invocation reruns selection
arms from their initial state; prior arm requests remain charged in the cumulative
ledger.

## Reports and cost accounting

`report.json` and `report.md` contain per-chat predictions, exact and tolerant
scores, and arm-level micro averages over completed chats. Episode-generation
usage is separate from arm usage. Reports include calls, input and output tokens,
cache-read and cache-creation tokens, known cost, unknown-cost call counts, errors,
and p50/p95 provider-invocation latency. Registry retries and repeated decision
samples count separately; HTTP continuations inside an adapter are aggregated
into that invocation's usage and latency. The cap is checked between provider
invocations, not between an adapter's internal continuations. Arm totals cover the current invocation; reused
episode stages retain their earlier generation cost. The report also includes
current-invocation and cumulative-ledger totals. Incomplete chats have no scored
result, and errors make the report status incomplete.
Per-arm `errors` include orchestration/validation failures; usage totals count
failed provider invocations, including failures followed by a successful retry.

`calls.jsonl` is an append-only attempt ledger with durable started/finished
records, candidate IDs, usage, and latency. A native decision endpoint's reported
cost takes precedence; otherwise retained usage is priced through the shared
list-price table. That fallback is an estimate, using peak pricing where a model
has peak hours because this benchmark supplies no billing timestamp. The known
`opencode-decisions/jev-1.13-free` route costs zero when usage is available.
Unpriced or missing usage stays unknown and blocks further attempts. Scripted
calls cost zero and cannot share an output directory with live calls.

Deciders retain the production default calibration (three samples and threshold
0.8 for these two routes), with a single selected model and no fallback. No arm
can silently switch models. The episode stage uses the configured production
LCM size limits with source-only replay input, chapter decisions disabled and
chapter rollover disabled.

## Verification limits

Synthetic tests cover canonical and legacy loading, record metadata, unknown
archive outcomes, label attachment and invalid labels. End-to-end fixtures cover
two chats across the decider arms, saved scores, frozen-stage reuse and a cost-cap
stop that preserves committed episode coverage. A read-only shape audit
of the five pinned fixtures found 177 eligible typed messages and valid label
references. An independent conversion comparison against the retained chat-a
canonical export matched role, text, tools and origin for all 1,728 records.
The historical five-chat scripted dry run, before removal of the tool-based arm,
completed with 74 episodes, 69 shared candidates,
and 557 calls (74 summaries, 207 per decider arm, 69 marks probes), with no errors.
Its private temporary inputs and outputs were deleted after verification.
These scripted checks do not establish live model boundary quality or downstream
task continuation quality. Private fixture text and model output are not repository
test fixtures.
