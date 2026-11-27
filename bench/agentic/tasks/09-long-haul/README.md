# 09-long-haul

An opt-in task about **recall across context compaction**. One job, about 1M tokens of history. Run it with
`--task 09-long-haul`; the default set stays the cheap 8 tasks.

## The job

An agent reads twenty "stages" (weeks) of a depot's receiving notes, `material/stage-01` to `stage-20`, and
writes one small file per stage, `out/stage-NN.json`, plus `out/final.json` at the end. The notes are
deliveries (weight, price per kilo), corrections, voids, and chatter, lab samples and price quotes that are
not deliveries. Suppliers appear by formal name, without the legal suffix, or by shorthand introduced in a
glossary in stage 1 (and more in stage 5).

Six rules are given **only in the prompt** and must hold in every stage file: supplier code naming, rounding
half to even per delivery, an exclusion list of three suppliers (also by shorthand), an ordering rule, a
two-delivery minimum, and a cumulative figure that carries over all earlier stage files. Later stages depend
on earlier ones twice: the shorthand is learned in stages 1 and 5, and `cumulative_cents` and `final.json`
sum the earlier files. A model that keeps its own notes file is playing fair; nothing forbids or requires it.

## What the check measures

`check.ts` scores per stage and per constraint, so the record shows where something was lost. `check_details`
of the record is a JSON string:

- `stages_correct`: stage numbers whose file equals the expected one exactly (`stages_missing`: no valid file).
- `constraints_kept_by_stage`: per constraint (`naming`, `rounding`, `exclusion`, `ordering`, `minimum`, `carry`)
  one character per stage: `1` kept, `0` lost, `-` no file. `first_lost` gives the first stage lost per constraint.
- `final`: whether `final.json` is present, correct, and consistent with the agent's own stage files.

A constraint is judged from the file alone where it can be: `rounding` looks at suppliers where half-up or
rounding once at the end would give another total; `exclusion` at any blocked code or code derived from a
blocked supplier's shorthand; `ordering`, `minimum` and `carry` against the agent's own rows. A lost
`naming` rule does not hide the others (a code that is a canonical one plus a suffix is still resolved for scoring).
`passed` needs all twenty stage files exact and a correct `final.json`: demanding on purpose, the details are the result.

## Volume: how the 1M tokens come about

- `setup.ts` writes the notes into the workspace (`prepareWorkspace` runs it after copying the seed). `world.ts` and
  `render.ts` are a pure function of a fixed seed: the same bytes every run, nothing committed as megabytes
  of text, about 90 MB of memory and 1.5 s to generate.
- Size: about 3.26 MB in 162 files (20 stages x 8 note files, about 160 KB per stage, about 20 KB per file, plus the glossaries).
  `runner.test.ts` pins it between 3.0 and 3.8 MB.
- Token estimate: no DeepSeek tokenizer is at hand, so 3.26 MB at 3.8 to 4.2 characters per token (English prose
  with numbers, ids and a layout that changes at stage 8 and 15) is **0.78 to 0.86M tokens of notes** read once. Add the
  read framing (about 3%) and the agent's own requests: per stage about 15 to 30 requests with a tool call and
  some thinking each, roughly 8 to 10k tokens, so about 0.15 to 0.2M over twenty stages. History before any
  compaction: **about 1.0M tokens (range 0.9 to 1.2M)**.
- Why the agent has to read: facts are spread over free text with several phrasings per field, three layouts
  (prose weeks 1 to 7, record blocks weeks 8 to 14, e-mail style weeks 15 to 20), corrections that supersede
  earlier values, voids, shorthand for suppliers, and look-alike notes (samples and quotes with weights and
  prices that must not be counted). A parser written for one layout does not carry over. An agent that writes a
  clever parser and reads little will accumulate less history; `requests`, `peak_request_input_tokens` and
  `compactions` in the record show what a run really did.
- Windows: BubblePaw compacts at the smaller of the cap and window minus its 16k output reserve, so about
  84k at `--context-window 100000` (roughly ten compactions for 1M) and about 234k at 250000 (several).

## Files

`task.json` (opt-in, setup hook, 90 min timeout, 1500 rounds), `prompt.md` (the rules), `seed/` (only
`out/`), `setup.ts`, `world.ts` (suppliers, notes, truth), `render.ts` (note text), `check.ts`, `reference.ts`
(the reference solution; writes the expected files, or loses one constraint from a stage on, for the tests).
Everything but `seed/` stays outside the agent's workspace.
