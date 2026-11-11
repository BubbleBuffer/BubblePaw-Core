# Workspace trust

**Status:** built 2026-11-10 (trust before launch). The in-host prompt that asks once per
workspace is a separate follow-up; until then every host behaves as headless.

A workspace's `.bubblepaw/config.toml` is **ignored until the workspace is trusted**. Opening
someone else's repository must not let its config change providers and endpoints, approval
rules, MCP servers, execution settings or anything else the user's own config decides.

## What is gated

- **Gated:** the project config layer, all of it, as if the file were absent. Nothing else in a
  workspace can execute code or redirect data: workspaces define no MCP files, hooks or commands.
- **Not gated:** prompt text. Memory files (`AGENTS.md`, `CLAUDE.md`) and `SKILL.md` catalogs keep
  loading; the model's approvals already bound what following them can do.
- A workspace with no project config needs no decision and never produces a warning.
- Invalid TOML in the project file is still an error, trusted or not.
- `remote.cloudflared` and `storage.data_dir` in a project file stay ignored even when trusted
  ([config](config.md#layers)).
- A project file cannot set `workspace_trust.*`: those keys are dropped with a warning in
  trusted workspaces and the file stays skipped in untrusted ones.
- `--config <file>` or `BUBBLEPAW_CONFIG` replaces the user and project files, so no project layer
  exists and there is nothing to trust.

## Granting trust (the user's decision only)

Trust is per workspace root (canonical path), exact: a subfolder of a trusted folder is its own
workspace.

| Way | Scope |
| --- | --- |
| `bubblepaw trust [PATH]` | Recorded in `<home>/.bubblepaw/trusted-workspaces.toml` (path and RFC 3339 time); every later process and host sees it. `--revoke` removes it, `--list` prints the records. |
| `--trust-workspace` | This run only. SDK: `ConfigOptions::trust_workspace()`. Env: `BUBBLEPAW_WORKSPACE_TRUST__TRUST_WORKSPACE=true`; `--set workspace_trust.trust_workspace=true` is the same key. |
| `workspace_trust.trusted = ["/path/to/repo"]` | User config: roots trusted without a store record. |
| `workspace_trust.enabled = false` | User config: turn the gate off and get the old behaviour for every workspace. |

The store lives in the user's home, never in a workspace or the data directory (which is only
known after the config has loaded). The SDK exposes `bubblepaw_sdk::workspace_trust` (`trust`,
`untrust`, `list`) for embedders that grant trust on the user's behalf.

## What a skipped project config looks like

- **Warning:** one line per file and process, naming the file, the top-level sections it sets and
  how to trust it; logged under the target `bubblepaw::workspace_trust` with `workspace`, `file` and
  `sections` fields.
- **Host API:** `config.get` carries `workspace_trust`: `{status, workspace, config_file, sections,
  how_to_trust}` with `status` one of `no_project_config`, `gate_off`, `trusted`, `untrusted`. A client
  can show it and later offer a Trust action.
- **Live reload:** the project layer stays skipped on every reload while untrusted. The trust store
  is watched with the config files, so trusting (for example `bubblepaw trust` in another terminal)
  takes effect at the next turn-start check or `config.reload`, with no restart; restart-only keys
  in the file then show as `pending_restart` like any other file change.

## Security note

The gate protects against a repository choosing the host's providers, endpoints, approvals and
tools. It does not sandbox a trusted workspace, and it cannot stop a user-approved shell command
from editing files in the home directory, including the trust store, just as such a command can edit
the user config. Trust is never granted by model output: no tool writes the store, the project layer
cannot set the keys, and `config.set` (a host-API method for clients acting for the user) is not
reachable from tools.

## Migration

Existing workspaces start untrusted. A headless or scripted run that relied on
`.bubblepaw/config.toml` now runs without it and logs the warning. One-line fix:
`bubblepaw trust` in the workspace (or `--trust-workspace` for a single run).
