# Host context hooks

Status: built 2026-10-21; hook reshaped 2026-10-22.

Built 2026-10-21 (P7 of the context-state work); the hook was reshaped on
2026-10-22. How a host that embeds BubblePaw shapes each main model request
through `ContextPreparation` (`crates/bubblepaw-core/src/runtime_preparation.rs`), and where
its per-request context goes. The cache rules behind this are in
[prefix-cache-design](prefix-cache-design.md).

## Where host context goes

| What | Where | API |
| --- | --- | --- |
| Static per-session instructions | system prompt (frozen tier) | `HarnessConfig.system_prompt`, `RunOptions.system_prompt` |
| Memory, skills and other derived prompt text | system prompt, frozen when the chat first binds | `HarnessConfig.system_prompt_resolver` |
| Live facts that change between requests (current project, processes) | tail reminder | `ContextWriter::remind(kind, text)` |
| Instructions that depend on the applied view | end of the cold tier | `ContextWriter::cold_note(kind, text)` |
| Runtime inventory | tail reminder `runtime_inventory` | written by the runner's `SummarizingContext` through `ContextWriter::remind`, and `remind_fresh` once after an LCM apply |

`RunOptions.system_prompt` stays the static per-session prompt. Never put live
facts into it or into the resolver's result: every change busts the whole cache.

## The preparation hook

The harness calls `prepare(call: PrepareCall<'_>)` once per main request (again
when steering, a newly queued reminder or final budget pressure requires it), and once
per manual compaction. `PrepareCall` carries:

- `session_id`, `request` (by value; the hook returns it in `PreparedContext`)
  and `provider`, the provider bound to this request;
- `writer`, a `ContextWriter` for the session's root lineage;
- `mode`: `PrepareMode::Request`, automatic `PrepareMode::Pressure {
  additional_tokens, additional_bytes }`, or `PrepareMode::Compact { focus }`,
  where `focus` is the host's manual compaction focus, if any;
- `cancellation`.

The default `prepare` passes a `Request` or `Pressure` through unchanged and fails a `Compact`
with "Manual compaction is not configured", so a host that does not compact keeps
working. A host that projects its history implements
`project_history(&self, history)`, which is an identity by default. Wrappers
(`UsageContext` here) forward `prepare` to their inner context.

A final `ContextBudgetExceeded` rejects the candidate before reminder delivery
and retries fresh history through `Pressure`, reserving the overhead absent from
the host's preparation estimate. A second recovery requires the serving frontier
to advance. The final validator remains read-only; gathering/apply belongs in
`prepare`. See [final admission recovery](lcm-contract.md#final-admission-recovery-2026-10-23).

`ContextWriter` is a narrow writer for the session's root lineage:

- `remind(kind, text)` queues `<harness kind="KIND">TEXT</harness>` for the
  request tail. The latest text per kind wins, text equal to the last sent value
  is not sent again (also after restart), and the flush is persisted with the
  request (see "Tail reminders (P2)"). A reminder whose text changed during the
  hook re-runs preparation once, so the new tail passes request admission.
- `remind_fresh(kind, text)` queues the text once even when it is unchanged. The
  runner uses it once after an LCM apply, so the facts appear exactly once.
- `cold_note(kind, text)` stages a note for the end of the cold tier. Notes are
  staged afresh for each preparation (the cold notes are cleared before the hook
  runs), and reach a lineage only at its first build or a declared change.
  After the hook returns, the harness stages the chat's own `tool_catalog`
  note through the same writer, so it follows the hook's notes.

The request arrives by value and goes back in `PreparedContext`. Editing
`system_prompt`, `tools` or `disabled_tools` there is reported, not hidden: the
harness fingerprints the frozen tier before and after the hook, and when the
hook's output changed while its input did not, the request's `context_built`
event is `frozen_change` with `declared=false` and
`frozen_writer: "context_preparation"`, and stderr gets a warning line.

## Migrating from the old hooks

- `prepare_request`, `prepare_request_with_provider`: use `prepare(call)`, with
  `call.request` and `call.provider`.
- `prepare_request_with_writer`: use `call.writer`.
- `compact_request`, `compact_request_with_provider`: handle
  `PrepareMode::Compact { focus }` in the same `prepare`.
- `runtime_notice`: removed. The runner writes the runtime inventory itself; a
  host that has its own live fact writes a reminder with `remind`.
- `cold_notes`: write each note with `ContextWriter::cold_note` in `prepare`.
- `prepare(&self, history)` (history projection): renamed `project_history`.

## System prompt resolver

`system_prompt_resolver(session, base)` returns the prompt for a chat. It runs
when the chat's surface (system prompt and tool declarations) is built, and the
result is persisted when the chat first binds, at its first admitted run
(`crates/bubblepaw-core/src/runtime_tools.rs`, `chat_surface`). Later runs reuse
the bound prompt, so a resolver that returns a different value later changes
only new chats: nothing is resent, and there is no declaration (no
`system_prompt_resolver` reason; the next request is an ordinary `append`). Put
facts that change during a chat into a reminder instead. The runner's
mode-aware resolver reads the chat's execution mode without binding it; the
mode binds at admission. There is no `ContextWriter::observe_system_prompt`.

## Episode worker template

`Harness::request_template(session)` is the request a default root run starts
from, without messages: the bound model, and the chat's system prompt and tool
declarations. Once the chat is bound these are the frozen surface, so a live
prompt or tool reload does not reach a bound chat's template; it reaches only
chats that bind later. Before the first admission the template previews the
surface a first run would bind, without persisting it or binding the execution
mode. The episode worker reads it per trigger. It observes and declares nothing.

## Tests

`crates/bubblepaw-core/tests/host_writer.rs`: hook reminders land at the tail and
are sent only when changed; a hook editing the system prompt is an undeclared
`frozen_change` with `frozen_writer`; a later resolver value applies only to new
chats and is not declared; a compaction
focus written in `Compact` mode reaches the next request's tail.
