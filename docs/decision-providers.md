# Decision providers

**Status:** Built; extended 2026-10-18 with OpenCode Jev and ordered failure
fallback. Two provider kinds sit in one registry:

- **LLM providers** generate text and tool calls (see [providers.md](providers.md)).
- **Decision providers** answer typed questions with probabilities.

Context strategies and approvals consume models through named **roles**. Each role is
resolved through the registry, so changing a model is a config change, not a code change.

Why: the episode decider was hardcoded to Jev over the OpenRouter Decisions API, with
an ad-hoc fallback to chat models. OpenAI now offers a Decisions API (`gpt-6-luna`).
Automatic tool-call checks need the same primitive.

## Built (2026-10-08)

Core now exposes `DecisionProvider`, typed questions and answers, and a separate
`DecisionUsage` carrying token counts and optional provider cost. The runner's
`ProviderRegistry` routes both provider kinds. `openrouter`, `openai` and `llm`
make one call per invocation; `ModelRole` owns repetition, averaging and bounded
transport retries. `ModelRoles` supplies the four ordered roles to episodes and
automatic approvals.

Built-in adapters are configured by `ProviderEndpoints` in
`crates/bubblepaw-runner/src/provider/endpoints.rs`: opencode chat has its base
URL, key environment name and reasoning fields; OpenRouter and OpenAI decisions
each have a URL and key environment name. Its `register` function adds every
adapter with a present key to the registry once in `main.rs`. Episodes and
approvals receive only roles and the registry; replay points these provider
endpoints at its metering relay. URL, key-name and reasoning-field validation
belongs to this provider boundary.

Details pinned while building:

- Summary and chapter default to `opencode/deepseek-v4.1-flash`, then the main
  registry model. Decider and approver have no built-in model chain: set
  `roles.decider` and `roles.approver` explicitly. A decision that needs a model
  with none configured fails naming that key. Approver uses one repeat and a 0.9
  threshold. Missing-key adapters are not
  registered and are skipped. After a candidate exhausts its configured retry
  policy, an explicitly configured next role candidate is tried. Known failed
  usage is retained; partial samples from a failed model are not averaged with
  the next model. Cancellation stops the sequence. Calibration uses the actual
  successful model.
- Split repeats and threshold follow the model selected at decision time
  (`ModelRole::decision_settings`, 2026-10-09): `openai/gpt-6-luna` uses one
  sample and ≥ 0.6, since it is deterministic and 0.6 was its best pipeline arm;
  every other decider keeps three samples and ≥ 0.8, including LLM fallbacks
  when Luna's key is missing. Explicit role repeat and threshold overrides win
  independently over these defaults. Live replay:
  the 2026-10-09 episode replay with Luna as decider, kept in the project notes.
- Chapter decisions consume the bounded accumulated open-chapter state and the
  newly completed episode summary. They choose whether that episode starts a new
  subject, rather than splitting inside an episode at a user turn. The exact
  prompt and options are recorded in the LCM worker record in the project notes.
  Native Jev uses a `state` and typed question/criteria map. OpenRouter carries
  `session_id` in JSON; OpenCode uses its header instead. Jev remains selectable
  with `--decider openrouter/typesafe/jev-1.13`. The local profile explicitly sets
  one sample and zero additional split threshold. A malformed LLM choice is a
  refusal at the adapter; the split role counts it as `none`.
- OpenAI posts to `/v1/decisions`, reads the answers array and reports input
  tokens only. Choice and score probability arrays are mapped to labeled pairs.
  A score is numeric, matching OpenAI's expected level index; an LLM level label
  maps to its zero-based index. The session identifier is sent as a header.
- The opencode chat provider passes through the configured `thinking` and
  `reasoning_effort` fields. Summary requests have no provider session, keeping
  stateful main-provider mirrors separate. Episode traces retain per-attempt
  token counts and reasoning; replay retains its HTTP attempt and cost meter.
  `Provider::generate_decision` defaults to ordinary generation; Responses
  overrides it to send the decision request's effort, default low. Explicit role
  effort reaches Messages, Responses and chat without changing the JSON prompt
  or parser. Reasoning stays on.
- Replay accepts a metered Jev/OpenRouter decider, `openai/gpt-6-luna`, or
  `llm/opencode/deepseek-v4.1-flash`, with DeepSeek Flash summaries/chapters. Luna uses
  the comparison's pinned $0.10/M input price and zero output charge, with a
  conservative reservation for unknown spend. Unpriced replay model overrides
  fail before dispatch. The normal runner's registry has no such model restriction.
- Auto uses an injected role provider, with threshold 0.9. Its input snapshots
  the consumed run request, includes the tool description and working directory,
  and bounds serialized arguments to 4,096 UTF-8 bytes. It records an ordinary
  `approved` row with `auto <model> p=<p>` before emitting the existing events.
  Below-threshold, refusal and provider errors fall through to Ask. Run
  cancellation interrupts the check and the ordinary cancellation path.
- CLI: `--decider`, `--approver`, `--summary-model`, `--auto-approve [model]`,
  and `--confirm-auto <tool,...>`. `--episode-summary-model` remains an alias;
  legacy endpoint, key-env and reasoning flags remain supported. A qualified
  summary model selects a registry provider; a bare name uses the configured
  opencode endpoint. Decision models require `<adapter>/<model>` or
  `llm/<provider>/<model>`; bare decision names are rejected. Per-tool modes
  retain precedence over `confirm`.

Acceptance uses scripted replay, injected providers and local fake HTTP servers.
No live Decisions or paid model calls are part of these checks.

## Interface (`bubblepaw-core`)

- `DecisionProvider::decide(DecisionRequest, CancellationToken) -> Result<DecisionResponse>`.
  Errors use the existing `ProviderError` kinds.
- `DecisionRequest { model, input: String, questions: Vec<Question>, session_id: Option<String>, reasoning_effort }`, effort defaults to low on old JSON and direct calls.
- `Question { name, instructions, kind }`. `kind` is one of:
  - `Predicate`
  - `Choice { choices: Vec<{ value, description }> }`
  - `Score { levels: Vec<{ label, description }> }`, levels ordered from lowest to highest
- `DecisionResponse { answers: Vec<Answer>, model, usage }`.
  - `Answer { name, kind }`. `kind` is one of:
    - `Predicate { probability }`
    - `Choice { choice, probabilities: Vec<(value, p)> }`
    - `Score { score, probabilities }`
    - `Refusal`
  - `usage` carries input and output tokens, plus `cost_usd` when the provider reports it.
- The request and response shapes follow the OpenAI Decisions API, the most general
  of the decision APIs. Each adapter maps to and from its own wire format.
- An adapter makes one call per `decide`. Repeats and averaging belong to the role
  (`repeats`, default 1), in one shared helper.

## Adapters

| Id | What | Key |
|---|---|---|
| `openrouter` | OpenRouter `alpha/decisions` (Jev `typesafe/jev-1.13`). Questions become the map keyed by name; choices become `criteria`. A predicate becomes a yes/no choice, and P(yes) is its probability. | `OPENROUTER_API_KEY` |
| `opencode-decisions` | OpenCode SystemOne decisions, configured with `kind = "opencode-decisions"`. Same typed criteria mapping; the session is sent in `x-opencode-session`, not a JSON `session_id`. Models `jev-1.13-free` and `jev-1.13` both returned successful live decisions on 2026-10-18. | `OPENCODE_API_KEY` |
| `openai` | OpenAI `POST /v1/decisions` (`gpt-6-luna`). Usage: input tokens only. | `OPENAI_API_KEY` |
| `llm` | `LlmDecisions` wraps any registry LLM model (`llm/<provider>/<model>`). It prompts for JSON answers with no tools. Its probabilities come from one sample: 1/0 for the parsed answer, and a malformed answer counts as a refusal. Role `repeats` turns these into vote shares. Reasoning is always on, low by default; explicit role effort overrides low. | that provider's |

Model names are `<adapter>/<model>`, for example `openai/gpt-6-luna` or
`openrouter/typesafe/jev-1.13`.

The local worker profile configures the OpenCode decision endpoint as
`https://opencode.ai/inference/systemone/v1/systemone`, with the free Jev model
first and the paid model second. Provider IDs are configurable; the registry
address uses the configured ID as its prefix. This does not route Jev through
the Chat Completions adapter.

Messages works through the same LLM route:
`llm/opencode-messages/claude-haiku-5-5`. A consumer can pass that model directly
to the registry's `DecisionProvider::decide`, or use role configuration:

```toml
[roles.summary]
models = ["opencode-messages/claude-haiku-5-5"]
reasoning_effort = "low"

[roles.decider]
models = ["llm/summary"]
repeats = 3
reasoning_effort = "low"
```

Config resolves `llm/summary` to the concrete Messages address. Each predicate,
choice or score answer is a hard vote. For example, two true predicates in three
samples produce confidence 2/3. This is vote share, not calibrated probability;
calibrated decisions remain on Luna/Jev. Native council/tool calls use the LLM
address `opencode-messages/claude-haiku-5-5` without the `llm/` prefix.

## Roles

| Role | Kind | Consumer | Default |
|---|---|---|---|
| `summary` | LLM | episode summaries, classical rolling summary | `deepseek-v4.1-flash` (opencode go) |
| `chapter` | LLM | chapter close and Related | same as `summary` |
| `decider` | decision | episode split | none built in; set `roles.decider` (for example `openai/gpt-6-luna`, then `llm/<summary>`, then `llm/<main>`); selected Luna: repeats 1, threshold 0.6; other models: repeats 3, threshold 0.8 |
| `approver` | decision | `auto` approvals | none built in; set `roles.approver`; one yes/no call, no repeats or threshold (setting either is rejected) |

- A role names one model or an ordered fallback list. A model whose key is missing
  is skipped.
- The configured list is the whole chain; nothing is appended. Include
  `llm/<main model>` in it explicitly if you want the main model as a last resort.
- Split when P(not none) reaches the selected model's threshold, choosing the
  most likely option. Explicit role configuration can override calibration.

## Automatic approvals

- `ApprovalMode` gains `Auto`. It is valid for `confirm` and for each per-tool entry.
- A call whose effective mode is `auto` sends the `approver` role the rendered
  approval prompt (`approvals.auto_prompt`, or the built-in default) as the
  decision input, with one yes/no question. The prompt's placeholders are
  `{user_request}`, `{tool}`, `{description}`, `{arguments}` (cut to 4 KiB) and
  `{working_directory}`; see [approvals](approvals.md#auto-mode).
- The default prompt asks whether the call stays within what the user asked for
  and is safe to run without a person looking.
- The answer decides (2026-10-24): one call on the first available model of the
  chain, no repeated samples, no threshold. Yes (p > 0.5) records `approved`
  with reason `auto <model> p=<p>`; the call proceeds and no one is asked.
- Otherwise the call falls through to `ask`. The approver never denies.
- Learning auto-approval from past decisions is deferred.
- A provider error or a cancellation also falls through to `ask`.
- In a one-shot run with no client, `ask` is denied as before.
- The events and the record are unchanged, so clients see an `approval_requested` that
  is already `approved`, followed by `approval_resolved`.
- CLI: `--auto-approve [model]` sets `confirm: auto`, and `--confirm-auto <tool,...>`
  sets single tools to `auto`.

## Not now

- Decision models in out-of-process extensions (a `decide` RPC method).
- Built-in tools that declare `confirm`. Without such declarations, `auto` only applies
  where a policy names it.
- Image input.

## Comparison

An earlier comparison, kept in the project notes, covers Luna against Jev
on the frozen boundary, pipeline, routing and approval sets.
