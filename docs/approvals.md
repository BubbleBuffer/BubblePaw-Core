# Tool-call approvals

**Status:** built through 2026-10-15. Code: `crates/bubblepaw-core/src/approvals/{mod,modes,rules,blocklist}.rs`. Configured deny rules and the hardline exec floor use scripted acceptance; Auto is retained without new work or live acceptance.

Approval modes apply live from config at the next turn, or immediately through
[reload](live-config.md). Every nested call resolves the current host policy;
explicit run overrides and child profile restrictions retain their scope. An
already admitted call keeps its decision. Auto mode also works when enabled
after startup; its approver role is resolved lazily at the gate.

A tool call can wait for a person's decision. This works for direct model calls and for `tools.*` calls inside `js`, which pass through the same gate. Nothing runs before approval: the gate sits after argument validation and before the effect row is written or `tool_started` is emitted.

## Tool metadata and policy

- `ToolDefinition.approval` is `none` (default) or `confirm`. No built-in tool declares `confirm` yet.
- `ApprovalPolicy { confirm, tools, deny, timeout_secs, hardline }` lives in `HarnessConfig.approval`. `RunOptions.approval` replaces modes and timeout for one run and can add denies; the host's configured denies still apply. The default is `confirm: ask`, no per-tool entries or denies, no timeout, and the [hardline floor](#hardline-floor) on.
- Each mode is `ask`, `allow`, `deny` or `auto`. Before any of these, the
  [hardline floor](#hardline-floor) is checked for `exec` commands. The
  effective mode for a call is, in order:
  1. a configured `deny` rule matching the tool: deny
  2. else a session rule left by "approve for session": allow
  3. else `tools[name]`
  4. else `confirm`, if the tool declares `confirm`
  5. else `allow`
- Automations set `confirm: allow` or `deny`; nothing else is needed.
- Child agents inherit the parent's effective call policy, including a scoped run override, tightened by their profile. Their deny list is the union of the parent's denies and the profile's own denies. Children cannot remove inherited denies; the same rule applies when continuing a child. The hardline setting is host-owned: children and run overrides take the host's value and cannot turn it off.

## Configured deny rules

```toml
[approvals]
deny = ["exec", "mcp__notes__delete_note"]
```

CLI overrides use the same table, for example `--set 'approvals.deny=["exec"]'`.

The rule grammar matches the existing session allowance grammar: **case-sensitive
exact tool names**. There are no wildcard patterns, command prefixes or argument
predicates. For example, `exec` matches only `exec`; `mcp__*` is a literal name
and does not match an MCP server's tools. Unknown names can be configured before
a tool is available. Blank entries and values other than a list of strings are
invalid.

A matching deny is checked before per-tool modes, `allow`, `auto`, session
allowances and run overrides, including the optimized single direct call path
and nested calls inside `js`. The ordinary denial record and events contain
`deny rule <tool name>`; the model's error names that rule. No matching tool
starts and no effect boundary is written for it. An existing session allowance
is preserved and can apply again after the configured deny is removed.

Deny changes reload with the other approval settings: next turn or explicit
`reload`, with a one-time policy diff notice. A run override keeps its modes
while resolving the current host deny list at each subsequent gate. Calls that
already crossed their approval gate retain their admitted decision.

The split keeps gate/persistence code in `mod.rs`, modes and the existing Auto
implementation in `modes.rs`, session allowances and deny matching in
`rules.rs`, and the hardline matcher in `blocklist.rs`.

## Hardline floor

```toml
[approvals]
hardline = true   # default
```

The floor is a fixed list of `exec` commands that never run, whatever the
mode, allowance, run override or Auto approver says. The gate checks it first:
before configured denies, session allowances, `tools[name]`, `confirm`, Auto and
run overrides. The direct single-call fast path never takes its `allow` shortcut
for a matching call, so that call goes through the gate. Nested `tools.exec`
calls inside `js` are checked the same way.

`hardline = false` turns the floor off. It is a host setting: the next turn or an
explicit `reload` applies it, with a one-time diff notice
(`approval hardline floor: on → off`). Run overrides and child profiles take the
host's value and cannot turn it off. Children inherit it from the parent.

Matched calls are recorded `denied` with reason `hardline <pattern>` and no
wait. No effect row is written and the tool does not start. The model gets an
error naming the rule:
`Blocked by hardline rule <pattern>. This command never runs, even when approvals allow it; choose a narrower command.`

### Fixed list

Only the `exec` tool is checked. Its `command` is a string, or a `{parts, values}`
template whose parts and values are joined with spaces. Patterns (the `<pattern>`
names):

| Pattern | Matches |
|---|---|
| `git reset --hard` | `git reset` with `--hard` anywhere after the subcommand |
| `git push --force` | `--force`, or a short-option cluster with `f` (`-f`, `-fu`) |
| `git clean -f` | `--force`, or a short-option cluster with `f` (`-f`, `-fdx`) |
| `rm -rf of <place>` | `rm` with recursive and force flags, targeting a protected place |
| `Remove-Item -Recurse -Force of <place>` | `Remove-Item` (or `ri`) with `-Recurse` and `-Force`, including prefixes such as `-r` and `-fo` |
| `mkfs` | a command word starting `mkfs`, such as `mkfs.ext4` |
| `dd to /dev/*` | `dd` with any `of=/dev/...` |
| `shutdown` | a `shutdown` command word, including `shutdown.exe /s` |
| `reboot` | a `reboot` command word |
| `format of a drive letter` | `format` (or `format.com`) with any drive-letter argument, such as `format C:` |

Protected places for the two delete patterns:
- **filesystem root:** `/`, `\`, `/*`, `\*`, `//`, and any path that climbs to it (`../../../..`);
- **home directory:** `~`, `$HOME`, `${HOME}`, `%USERPROFILE%`, `$env:USERPROFILE`,
  and their contents (`~/*`, `$HOME/*`);
- **drive root:** `C:`, `C:\`, `c:/`, `C:\*`;
- **workspace root:** the session's exec directory root, as an absolute path, a
  relative path that resolves to it (`.` from the root, `..` from `child/`), or a
  `\\?\` verbatim path. The root is the session's worktree when it has one, and the
  host's bound workspace is protected too. It is the directory itself, not its
  contents.

The workspace checks resolve each target against the exec directory: the `cwd`
argument (default: the root), moved by any `cd`, `chdir`, `pushd` or `Set-Location`
earlier in the same command. A `cd` to a home path, a variable or `-` makes the
directory unknown; then `.` is still taken as the root, so the check fails closed.
Without a bound workspace, only the literal `.`, `./` and `.\` forms match.

`git push --force-with-lease` and `--force-if-includes` are allowed. Words inside
other commands' arguments do not match, so `echo "rm -rf /"` and
`git commit -m "git reset --hard"` run.

Normalisation is best effort. The matcher lowercases the text, removes carets,
backticks and braces, and splits segments at newlines and `; & | ( )`. Quotes are
read two ways, and both readings must pass: one where a quoted path with spaces is
one word, and one where quotes only vanish.
Before each command word it skips wrappers (`sudo`, `doas`, `env`, `nohup`,
`time`, `timeout 5`, `xargs`, `cmd /c`, `sh -c`, `powershell -Command`, and
similar) and `NAME=value` assignments. It ignores the directory part and a
`.exe` suffix.

This is insurance, not a sandbox. It reads command text only, so it does not
catch:
- a wrapper with a value option before the command (`sudo -u root rm -rf /`);
- `del /s` or `rd /s` from cmd, or other shell or interpreter forms that hide the command;
- the contents of the workspace (`rm -rf *`, `rm -rf ./*`) or of its parent;
- a directory the shell moves into through a variable, `~` or `cd -` (only `.` is still caught);
- `cd..` or `cd\` written without a space;
- `+refspec` force pushes;
- MCP tools or other host tools that run commands;
- aliases, scripts or files that contain a dangerous command.

## Auto mode

Auto (built 2026-10-08) checks the `approver` decision role before waiting for a
person. There is no built-in approver chain: set `roles.approver` (for example
`openai/gpt-6-luna`, then `llm/<summary>`, then `llm/<main>`). With none configured,
auto asks the person and logs a warning.

The approver's answer decides (2026-10-24). The gate renders the approval
prompt and sends it to the approver as the decision input with one yes/no
question, in one call: no repeated samples and no probability threshold. The
chain order in `roles.approver` is the user's: a decision provider first, an
`llm/` model only as its fallback. Yes (for a predicate probability, p > 0.5)
records `approved` with reason `auto <model> p=<p>`. No, refusal or provider
failure falls through to Ask (a failure also logs a warning); Auto never denies.
`roles.approver.repeats` and `roles.approver.threshold` are rejected by config
validation. Learning auto-approval from past decisions is deferred.

The prompt is the template in `approvals.auto_prompt`, read live at each gate
(it follows config reload). Unset, the built-in default
(`bubblepaw_core::DEFAULT_AUTO_PROMPT`) asks whether the tool call stays within
what the user asked for and is safe to run without a person looking, and lists
the five fields. Placeholders are plain text substitution in one pass:
`{user_request}` (the consumed run request), `{tool}`, `{description}` (the tool's
description), `{arguments}` (bounded to 4 KiB, then marked truncated) and
`{working_directory}`. Any other `{...}` text is left as is. Library embedders set
`AutoApprovalConfig.prompt`. A run cancellation interrupts the check and cancels
the pending gate. The ordinary events are retained: `approval_requested` already
`approved`, then `approval_resolved`. There is no schema change.

## Records and states

- Table `approvals(id, session_id, run_id, call_id, tool, arguments, state, reason, created_at, decided_at)`; created with `IF NOT EXISTS` on every open outside the version ladder. The merged episode/view schema remains v6.
- `state` is `pending`, `approved`, `approved_session`, `denied`, `expired` or `cancelled`. The store record is the truth; an in-memory waiter only wakes the gate. Waiters are shared by every harness on one store, so a parent harness can decide a child agent's approval.
- Table `approval_rules(session_id, tool)` holds "approve for session". It persists and applies to later runs of that session only.
- `call_id` is the provider's call id for direct calls, and the dispatcher's fresh id for nested calls.
- The run stays `running` while it waits.

| What happens | State | Reason | Result for the model |
|---|---|---|---|
| Approve | `approved` | | the call proceeds |
| Approve for session | `approved_session` | | the call proceeds; a session rule is added |
| Deny | `denied` | the person's text, if any | `Denied by the user: <reason>. Do not retry this call unless the user asks.` |
| Policy `deny` | `denied`, no wait | `policy` | as for a denial |
| Configured deny rule | `denied`, no wait | `deny rule <tool name>` | denial naming the matching rule |
| Hardline floor match | `denied`, no wait | `hardline <pattern>` | `Blocked by hardline rule <pattern>. This command never runs, even when approvals allow it; choose a narrower command.` |
| Timeout (`timeout_secs`) | `expired` | `timeout` | `Approval timed out.` |
| Run or session cancelled, or the cell aborted | `cancelled` | | the call site's cancellation error; the run ends `cancelled` |
| Restart with a pending approval | `expired` | `restart` | the run is `interrupted` as before |

- A denied direct call takes the rejected-call path: no effect row and no `tool_started`, but an error tool message and `tool_finished`.
- A denied nested call returns a tool error, so the JS `await` rejects and the cell can catch it.
- Deciding an approval twice, or an unknown id, is an error.

## Events

- `approval_requested { approval }` carries the full record. A policy denial emits it with the record already `denied`, then `approval_resolved`.
- `approval_resolved { approval_id, state, reason }`.
- Both are durable events in the store's one event log and stream, which child agents share with their parent ([session API](session-api.md)). A decision's `approval_resolved` carries the deciding client as `origin` when the command named one.

## Timeouts elsewhere

Time spent waiting for approval does not count against the `js` cell idle limit or the non-wait host call deadline (both 30 s). Cells track waits with `track_approval_waits`; while any call in a cell waits for a person, both clocks pause. Active JS time is unaffected because no JS runs while waiting.

## APIs

- Core: `Harness::pending_approvals(session_id)` and `Harness::decide(approval_id, Decision)` (or `decide_from` with a `ClientInfo`), where `Decision` is `Approve`, `ApproveForSession` or `Deny { reason }`.
- stdio: `approval.list { session_id }` and `approval.decide { approval_id, decision: "approve" | "approve_session" | "deny", reason?, client? }`. Events arrive through the `event` notification.
- Web: `GET /api/sessions/{id}/approvals`, `POST /api/approvals/{id}` with the same body. The session view includes pending `approvals`.
- CLI: `--confirm <tool,...>` sets `tools[name] = ask`; `--approve-all` sets `confirm: allow`; `--auto-approve [model]` sets `confirm: auto`; `--confirm-auto <tool,...>` sets `tools[name] = auto`. `--approver <adapter/model>` selects the role's first model, and an optional model on `--auto-approve` overrides it. In one-shot `run` and `resume`, no client can answer, so a call that would wait is denied at once with the reason `no client to approve; use --approve-all`.

## Limits

- Child agents' approval events reach the parent's subscribers, with the child's session id; `approval.list` on the child session shows them.
