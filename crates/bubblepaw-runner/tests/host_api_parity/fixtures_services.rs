//! Fixtures for the services group: startup and config, providers and roles,
//! storage, environments, processes, approvals, attachments and artifacts.
//! Anything a fixture changes it restores before it ends, so the second
//! transport starts from the same services the first one saw.
use super::fixtures::*;
use serde_json::json;

/// A scripted provider that exists only for the duration of its fixture.
fn scratch_provider(name: &str) -> serde_json::Value {
    json!({
        "name": name,
        "input": {"kind": "scripted", "models": [{"id": "first"}]},
    })
}

pub fn all() -> Vec<Fixture> {
    vec![
        fixture("initialize", vec![ok("initialize", json!({}))]),
        fixture(
            "config.get",
            vec![
                // The whole list carries per-host values (the data directory), so only its structure is compared.
                ok("config.get", json!({})).shape(),
                ok("config.get", json!({"key": "agents.wake_debounce_ms"})),
                err(
                    "config.get",
                    json!({"key": "no.such.key"}),
                    "Unknown config key",
                ),
            ],
        ),
        fixture("config.reload", vec![ok("config.reload", json!({}))]),
        fixture("providers.list", vec![ok("providers.list", json!({}))]),
        fixture(
            "providers.save",
            vec![
                ok("providers.save", scratch_provider("parity-$role")),
                ok("providers.remove", json!({"name": "parity-$role"})),
            ],
        ),
        fixture(
            "providers.remove",
            vec![
                ok("providers.save", scratch_provider("parity-$role")),
                ok("providers.remove", json!({"name": "parity-$role"})),
            ],
        ),
        // The scripted `demo` provider is local and offline, so the test never
        // reaches a real endpoint.
        fixture(
            "providers.test",
            vec![ok("providers.test", json!({"name": "demo"}))],
        ),
        fixture("providers.reload", vec![ok("providers.reload", json!({}))]),
        // After the provider fixtures: a settings write rewrites the file they compare against.
        // Each change is reverted in the same fixture, so the next transport starts clean.
        fixture(
            "config.set",
            vec![
                ok(
                    "config.set",
                    json!({"key": "agents.wake_debounce_ms", "value": 77}),
                ),
                ok("config.get", json!({"key": "agents.wake_debounce_ms"})),
                ok(
                    "config.set",
                    json!({"key": "agents.wake_debounce_ms", "value": null}),
                ),
                err(
                    "config.set",
                    json!({"key": "agents.wake_debounce_ms", "value": "soon"}),
                    "invalid type",
                ),
                err(
                    "config.set",
                    json!({"key": "no.such.key", "value": 1}),
                    "Unknown config key",
                ),
            ],
        ),
        fixture(
            "roles.set_main",
            vec![
                ok("roles.set_main", json!({"model": "demo/second"})),
                ok("roles.set_main", json!({"model": "demo/first"})),
            ],
        ),
        fixture(
            "storage.stats",
            vec![ok("storage.stats", json!({})).shape()],
        ),
        fixture(
            "environment.inspect",
            vec![
                open("environment", "s"),
                ok("environment.inspect", json!({"session_id": "$s"})),
            ],
        ),
        fixture(
            "environment.control",
            vec![
                open("environment-control", "s"),
                // No control action is given, so the default inspect action runs on both transports.
                ok("environment.control", json!({"session_id": "$s"})),
            ],
        ),
        fixture(
            "process.list",
            vec![
                open("process", "s"),
                ok("process.list", json!({"session_id": "$s"})),
            ],
        ),
        fixture(
            "process.background",
            vec![
                open("background", "s"),
                // No process has this handle, so the call is refused on both transports.
                any_err(
                    "process.background",
                    json!({"session_id": "$s", "handle": "parity-missing-handle"}),
                ),
            ],
        ),
        fixture(
            "approval.list",
            vec![
                open("approval", "s"),
                ok("approval.list", json!({"session_id": "$s"})),
            ],
        ),
        fixture(
            "approval.decide",
            vec![
                // A valid decision for an approval that does not exist, so the call is
                // refused by the approval lookup on both transports.
                any_err(
                    "approval.decide",
                    json!({"approval_id": "parity-missing-approval", "decision": "deny"}),
                ),
            ],
        ),
        fixture(
            "attachments.add",
            vec![
                open("attachments-add", "s"),
                ok(
                    "attachments.add",
                    json!({"session_id": "$s", "name": "parity-$role.txt", "data": "cGFyaXR5"}),
                )
                .bind("att", "/id"),
            ],
        ),
        fixture(
            "attachments.read",
            vec![
                open("attachments-read", "s"),
                ok(
                    "attachments.add",
                    json!({"session_id": "$s", "name": "parity-$role.txt", "data": "cGFyaXR5"}),
                )
                .bind("att", "/id"),
                ok(
                    "attachments.read",
                    json!({"session_id": "$s", "id": "$att"}),
                ),
            ],
        ),
        fixture(
            "artifacts.list",
            vec![
                open("artifacts-list", "s"),
                ok("artifacts.list", json!({"session_id": "$s"})),
            ],
        ),
        fixture(
            "artifacts.read",
            vec![
                open("artifacts-read", "s"),
                // No artifact has this id, so the call is refused on both transports.
                any_err(
                    "artifacts.read",
                    json!({"session_id": "$s", "id": "parity-missing-artifact"}),
                ),
            ],
        ),
        // MCP: the parity config has no servers, so sign-in controls report
        // the unknown server; a real sign-in is covered by the MCP crate tests.
        fixture("mcp.servers.list", vec![ok("mcp.servers.list", json!({}))]),
        fixture(
            "mcp.auth.start",
            vec![err(
                "mcp.auth.start",
                json!({"server": "missing"}),
                "MCP server missing is not configured",
            )],
        ),
        fixture(
            "mcp.auth.complete",
            vec![err(
                "mcp.auth.complete",
                json!({"server": "missing", "redirect_url": "http://127.0.0.1:1/callback"}),
                "MCP server missing is not configured",
            )],
        ),
        fixture(
            "mcp.auth.sign_out",
            vec![err(
                "mcp.auth.sign_out",
                json!({"server": "missing"}),
                "MCP server missing is not configured",
            )],
        ),
    ]
}
