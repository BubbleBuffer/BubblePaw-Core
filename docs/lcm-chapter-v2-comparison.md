# LCM chapter prompt v2 comparison

Newest round first. Both rounds compare chapter prompt v1 with v2 (PR #27) on the
same five frozen chapters, with `bubblepaw-episode-prompt-bench --chapter-fixtures`
and DeepSeek V4.1 Flash only. Each round is one live sample per chapter and arm.
Both rounds together cost $0.107.

## Chapters

| Chapter | Episodes | Source |
|---|---|---|
| typed-decision | 1–2 | checked-in synthetic fixture (`tests/fixtures/chapter-prompt-bench.json`) |
| delegated-work | 7–8 | checked-in synthetic fixture, delegated task, no typed request |
| 791b9ca4 | 1–7 | recorded chat (SQLite migrations), v2 episode summaries from episode round 1 |
| 94af8a96 | 1 | recorded chat (panic supervision in `Harness::drive`), v2 summary from episode round 3 |
| 7a533468 | 1–3 | recorded chat (runner async offload), v2 summaries from episode round 3 |

The three recorded chapters reuse the chats of
[lcm-episode-v3-comparison](lcm-episode-v3-comparison.md), with their v2 episode
summaries (the default episode prompt) under production `EPISODE N [source ...]`
headers, and the chat's typed requests as request evidence. They contain private
chat text, so they are not checked in; the frozen file stays with the private
bench outputs. There are no earlier chapters in any fixture.

## Round 2: with recorded v2 blocks (2026-10-29)

The bench now records, for the v2 arm, the raw text after the prose
(`metadata_text`) and the parser error (`metadata_error`). Round 1 could not
explain its fallbacks because the runner strips the blocks and only logs the
reason. 10 calls, $0.042, cap $0.25.

| Chapter | v2 parsed | Fallback reason | Decisions | Supported | Sections | Output tok v1 → v2 | Summary chars v1 → v2 |
|---|---|---|---|---|---|---|---|
| typed-decision | yes | | 1 | 1/1 | 2: "Inspect and preserve reader" 1–1, "Implement path fix" 2–2 | 569 → 256 | 1385 → 1025 |
| delegated-work | yes | | none | correct: nothing was decided | 1: "Attachment fixture investigation" 7–8 | 467 → 1217 | 917 → 1309 |
| 791b9ca4 | **no** | `invalid_section` | 4 (unparsed) | 4/4 | 3, one written `episode 5` | 3859 → 3918 | 9736 → 7901 |
| 94af8a96 | yes | | 1 | 1/1 | 1: "Panic supervision investigation" 1–1 | 2163 → 2423 | 7358 → 7251 |
| 7a533468 | yes | | 1 | 1/1 | 2: "Verify defects and host attach" 1–2, "Cache, git async, checks" 3–3 | 4613 → 3931 | 10790 → 8521 |
| **All** | **4/5** | | 7 | 7/7, 0 invented | | 11671 → 11745 (+0.6%) | 30186 → 26007 (−14%) |

The one fallback, as the model wrote it:

````text
Sections:
- Reconnaissance and version design | episodes 1-4
- Core numbered migration edits | episode 5
- Fixtures, tests and docs | episodes 6-7
````

The prompt's form is `episodes <first>-<last>`, so `episode 5` breaks the
grammar; the parser needs the literal ` | episodes ` separator and returns
`invalid_section`. The decisions and the two other rows were valid, and the three
sections tile episodes 1–7 at the right topic changes. One natural shorthand for a
single-episode section discards the whole block.

**Decisions.** All seven name a decision the episodes or typed requests record:
for example "Keep the existing reader and fix only the import path conversion.",
"Stopped the worktrees sub-item rather than restructuring the sync start_worker
API." and, in 94af8a96, the settled design from the request ("Supervise the
spawned drive via a wrapper task holding the JoinHandle; add no new
dependencies."). That one picks the wrapper option where the request allowed
either a wrapper or `catch_unwind`, a slight narrowing rather than an invention.
`Decisions: none` for the delegated child that only reported missing evidence is
correct. None is over 100 characters.

**Sections.** All names say what happened, in plain words, without quips. The
boundaries follow the episode titles: 791b9ca4 splits reconnaissance (1–4), core
edits (5) and fixtures, tests and docs (6–7); 7a533468 splits verification and the
Ctrl-C edit (1–2) from the cache, git and checks work (3). The synthetic
two-episode chapter was split per episode, which is defensible but finer than
needed.

## Round 1: first live run (2026-10-29)

Same five chapters, before the bench recorded the v2 blocks. Two invocations
(synthetic and recorded chapters), 10 calls, $0.065.

| Chapter | v2 parsed | Decisions | Supported | Sections | Output tok v1 → v2 | Summary chars v1 → v2 |
|---|---|---|---|---|---|---|
| typed-decision | yes | 1 | 1/1 | 1: "Import repair" 1–2 | 512 → 498 | 2288 → 518 |
| delegated-work | **no** | – | – | – | 380 → 385 | 1031 → 1578 |
| 791b9ca4 | **no** | – | – | – | 4825 → 5569 | 12132 → 9011 |
| 94af8a96 | **no** | – | – | – | 3204 → 2167 | 8163 → 7396 |
| 7a533468 | yes | 4 | 4/4 | 2: "Findings and Ctrl-C listener" 1–2, "Cache, async git, worktrees stop" 3 | 3268 → 3838 | 9288 → 7055 |
| **All** | **2/5** | 5 | 5/5, 0 invented | | 12189 → 12457 (+2.2%) | 32902 → 25558 (−22%) |

The three fallbacks have no recorded reason. Each response did contain a line
starting with `Decisions:` or `Sections:`, since the prose was cut there, so the
blocks were present but failed one of the parser's rules. Round 2's
`invalid_section` from a single-episode row is the likely common cause, but this
is not shown for round 1.

## Description and summary

The extra blocks did not make the shared parts worse in either round. The
descriptions name the same subject in every chapter; v2's are shorter (379 and
382 characters in total against 421 and 545), and one was in Title Case. v2's
summary prose is 14–22% shorter in total and shorter in 4 of 5 chapters each
round; only the delegated-work chapter grew. The long chapters keep the same
parts at heading level (goal and constraints, state per sub-item, verification,
open work); v2 merges headings, for example "Verification limits and open work".
Output tokens stay level (+2.2% and +0.6%) because they include the model's
reasoning; the blocks themselves add only a few hundred characters. A line-by-line
fact comparison of the prose was not done.

## Fix (applied in this PR)

The fallbacks seen come from a strict section row form, not from invented
content. This PR applies both changes that would have parsed round 2's fallback:

- **Parser (applied):** `- Name | episode N` is read as `episodes N-N`, and an
  en dash (–) is accepted in a range as well as a hyphen. The tiling check and
  every other rule are unchanged, and a real error still discards the whole
  block. `episode 7-10` (a range after the singular) is still rejected.
- **Prompt (applied):** the v2 chapter prompt now says "For a single episode
  write `episodes N-N`." The prompt text changes no stored data, so there is no
  new version.

Tests cover the exact failing block (`episodes 1-4` / `episode 5` /
`episodes 6-7`, three sections tiling 1–7), an en dash range, and gaps and
overlaps still failing. The fixed parser has not been re-run against live
output, since this PR makes no model calls.

Not changed: the all-or-nothing fallback. Keeping the decisions when only the
Sections block fails would change it and needs a separate decision.

## Verdict

v2 is good enough to try in the compacted-history trial with the single-episode
row form now accepted. All 12 decisions checked were grounded, none
invented, sections were well named and placed, output size stayed level and the
shared prose did not get worse. The parse rate (2/5, then 4/5) is the blocker: an
all-or-nothing parse loses the retrieval blocks for whole chapters on a minor form
slip. Two samples of five chapters carry large run-to-run noise.
