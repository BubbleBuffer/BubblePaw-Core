# Classical summarization (rolling summary)

Status: built 2026-10-09. A second summarization strategy next to episodes and
chapters (the earlier episode design, kept in the project notes), so the two can be swapped and
compared on the same replay. The raw research notes, with sources, are kept in the project notes (literature, industry, evaluation, prompts).

## Built (2026-10-09)

Sessions select `lcm` (default) or `classical` when created.
`bubblepaw run "..." --context-strategy classical` creates a classical session;
`bubblepaw replay --chat <corpus.json> --out <fresh-dir> --dry-run
--context-strategy classical` selects a replay arm. Resuming without the flag uses
the saved strategy; explicitly requesting a different strategy rejects the run.
Create a new session or replay archive to change it. The replay command needs a build with `--features bench`. Forks inherit the strategy
and start with a fresh disposable view.

Manual `/compact [focus]` is available only for LCM sessions. Classical sessions
reject it without changing their selected strategy or checkpoint. Automatic classical
summarization remains unchanged.

Applied classical compaction rebinds the skill catalog and memory-file prefix
from current state, just like [LCM apply](lcm-contract.md#apply). Covered change
notices leave the serving view; the durable snapshot becomes the baseline for
later notices. Prefix bytes stay fixed between compactions.

The core `ContextPreparation` boundary has an optional explicit-compaction hook. The runner uses one
`ContextSummaryStrategy` interface with two concrete policies, sharing
`SummarizingContext`, its production O200kBase estimator, threshold
`min(window - 16,000, 256,000)`, complete-round frontiers, projections and frozen
views. LCM delegates to the existing episode/chapter rendering and retains its
prompts, planner and decisions. Original messages and the session-local
`history_search` / `history_read` tools remain available in every arm.

Classical updates use the previous rolling summary and only the newly covered
span, with the existing aggressive episode summarizer masking. The anchored
prompt is `crates/bubblepaw-runner/src/classical_prompt_v1.txt`; its sections are
Latest user request (verbatim), Goal, Constraints and decisions, Done (dated,
past tense), In progress, Stale or superseded asks, and Next step. The `summary`
role supplies the model with low reasoning. Length is guidance: output is kept
intact, without length-based retries or truncation, subject to the shared 64 KiB
runaway-output backstop.

The existing per-session worker activates at half the threshold. Classical
precomputes an advancing checkpoint without changing provider input, then
publishes it at the threshold. A ready checkpoint does not wait for background
work. Background work keeps one unused pending checkpoint instead of rewriting
it on every committed message; it resumes after that checkpoint is applied.
Missing coverage or a pending checkpoint that cannot fit the next view uses
bounded synchronous catch-up.

Classical includes covered typed-user messages newest first, retaining
whole messages verbatim within a 20,000-token block budget. Older messages become
one-line ID references resolved by `history_read({message_id: ...})` or search.
If an exceptionally large archive's ID index alone exceeds that budget, every
reference is retained and the block explicitly identifies the capacity limit.
The raw tail remains outside that block. Raw-zone observation masking and
argument clipping remain shared with LCM. The request byte guard still applies.

Schema stays **v6**. Strategy is a serde-defaulted field in session JSON; old
sessions read as LCM without rewriting their stored bytes. Classical uses the
existing v3 `context_checkpoints` row, with a serde-defaulted `files_touched`
list merged from structured read/edit/write tool-call path arguments in each new
span. The rendered summary appends this deterministic list. Opaque JavaScript
source and transient nested reads do not supply a persisted file trail; paths
are never inferred from prose or code strings. Canonical history stays untouched.

Replay keeps the existing reports and saved view text, adding `context_strategy`,
`compaction_count`, `view_tokens_per_turn` (at typed user requests),
`rolling_checkpoint`, and `totals.summary` calls, input/output tokens and cost.
Scripted classical token figures are local estimates; scripted calls cost zero.
No main-model continuation, quality probes, or live/paid model acceptance is
claimed by these reports.

Initial offline verification on 2026-10-09 passed formatting, strict workspace Clippy,
349 workspace tests and 366 all-features tests. Both scripted arms completed
the existing 1,728-message private chat corpus (not reproducible) and the eight-message fixture.
The private chat used 7 / 2 summary calls and 9 / 4 store transactions for
LCM / classical, excluding canonical imports and schema initialization.
LCM decisions, episodes, chapters and normalized saved views matched `da44929`.
Generated UUIDs cause small token-count differences even between unchanged HEAD
runs; those derived metrics are checked separately from view content.

## How it is generally done (researched 2026-10-08 and 2026-10-09)

Fifteen coding agents, about 15 papers and the main industry write-ups. The common
recipe has five parts:

1. **Shrink tool output first.** Nearly everyone truncates or masks old tool
   results before any LLM call (Cline and Kilo cut to 2,000 chars, Gemini uses a
   budget, OpenHands and SWE-agent mask old observations, Anthropic clears old tool
   results). Strongest coding evidence: JetBrains (arXiv 2508.21433) on SWE-bench,
   masking alone matched LLM summarization on solve rate (54.8 vs 53.8, raw 53.4) at
   about half the cost.
2. **Trigger at a fraction of the window**: Gemini 0.5, Goose 0.8, Cline and Codex
   0.9, Anthropic default 150k. None of these thresholds is validated against
   quality data.
3. **Update the previous summary, don't redo it.** OpenHands, Cline, Gemini, Roo,
   Aider, opencode, Hermes and Kilo pass the old summary in ("keep still-true, drop
   stale, merge new"). Full rewrites lose detail. ACE (2510.04618) measured a
   rewrite collapsing 18,282 tokens to 122, with accuracy falling from 66.7 to 57.1.
   No paper measures drift over many incremental updates.
4. **Keep a verbatim tail, and often the user's own words.** Codex keeps recent
   user messages verbatim (20k tokens). Roo and Goose require "all user messages" in
   the summary. Hermes keeps the latest request verbatim. Evidence: verbatim beats
   extracted memory by 16 to 22 points (2601.00821, LoCoMo and LongMemEval).
5. **Structured sections**: goal, constraints, decisions, done, in progress,
   next step, files. Every method scores worst on the **artifact trail** (which files
   were touched; Factory probes, under 2.5 of 5 for all). Cline therefore injects
   file lists from tool records instead of asking the model.

Where it diverges:

- **Segments vs one rolling summary.** On long-chat QA, topic segments beat a
  recursive rolling summary: SeCom (2502.05589) scored 69.3 vs 56.3 on LoCoMo.
  ReadAgent and MemGPT keep gists or pages plus retrieval of originals. No study
  tests this on coding agents.
- **Summarize at all?** Amp removed compaction in favour of user-started handoffs to
  a new thread. Cursor trains the model to summarize itself. SWE-agent only masks.
- **Sync vs background.** Most agents block the turn. Aider summarizes in a
  background thread with a weak model. Letta's sleep-time compute precomputes
  between turns.
- **Errors.** Manus keeps failed actions in context. Letta finds that full failed
  histories hurt recovery most (26.3% to 11.2%). The useful thing to keep is the
  lesson, not the trajectory.

Known failure modes: summary-of-summary drift, stale asks replayed as open (Hermes
marks them STALE), lost identifiers and paths, and a slow, expensive call at the
worst moment.

## What this means for us

- What we already do matches the consensus: aggressive masking before summarizing,
  originals kept, retrieval by id, incremental episode summaries, and background
  work from half the threshold.
- The literature supports retrieval of originals plus segment-level gists over one
  rolling summary. Nothing yet shows that our extra machinery (decider-driven
  splits, chapters, Related links) beats a simple rolling summary with masking and a
  history search tool. That is the comparison to run.

## Proposal

Reuse everything the episode strategy already has except the summary block:
threshold, frontier, raw-zone masking, frozen view, retained originals and history
retrieval by id.

- **Input:** previous summary plus the span between the previous and new frontier,
  rendered with the same aggressive summarizer masking as episodes. No re-reading of
  older history.
- **Prompt:** anchored update ("keep still-true details, drop stale ones, merge new
  facts"). Sections: latest user request (verbatim), goal, constraints and
  decisions, done (dated, past tense), in progress, stale or superseded asks, next
  step. Length is guidance only.
- **Files touched** are listed deterministically from tool records, not written by
  the model.
- **Storage:** one rolling summary per session (the v3 `context_checkpoints` table
  fits), originals untouched.
- **Strategy is fixed per session at creation.** Changing it starts a fresh view.

Recommended answers to the three open questions:

1. **Verbatim user messages:** yes. Keep the most recent typed messages up to about
   20k tokens and refer to older ones by id.
2. **Blocking:** no. Precompute in the background from half the threshold, as the
   episode worker does, and apply at the threshold. Block only if it is not ready.
3. **Summary model:** the same `summary` role for both strategies (DeepSeek Flash), so the
   comparison isolates strategy.

## Comparison arms

On the same recorded chats, with the same summary model and judge:

| Arm | What it is |
|---|---|
| Classical | Rolling summary, tool-output masking, verbatim user messages and tail, history retrieval |
| LCM | Episodes, chapters, decider, Related, raw-zone masking and history retrieval |

Measures, cheapest first:

- Free metrics: tokens per turn, cache-read share, writes, stored bytes.
- Hand-keyed probes. Factory-style recall, file path, decision and continuation
  probes, plus **uncued recall**: questions that need an earlier result without
  naming it.
- Probes at fixed typed-user-turn positions with the same eligible questions in
  both arms; compaction counts remain a covariate.

If LCM does not beat classical on these probes, cut the machinery that does not pay.

Paid runs need a go-ahead with a cost estimate.
