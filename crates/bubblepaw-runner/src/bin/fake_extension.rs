//! Test fixture: a provider extension speaking the docs/providers.md protocol.
//! The last non-notice message of each step selects the behaviour:
//! - a tool message: reply `tool result: <content>`;
//! - `text a|b|c`: stream the pieces as `text` notifications, then reply with them joined;
//! - `text-delay <ms> a|b`: stream pieces, wait, then reply;
//! - `text-error a|b`: stream pieces, then fail transiently;
//! - `call [name]`: return one tool call (echo by default);
//! - `bad-call null|schema`: return an echo call with invalid arguments;
//! - `js <code>`: return one `js` tool call with the supplied script;
//! - `error <auth|rate_limit|context|transient|fatal>`: fail with that kind;
//! - `usage-error <kind>`: fail with that kind, carrying reported usage (5 in, 2 out, 3 cache read);
//! - `raw-error <kind>|<text>` / `option-error <option>|<text>`: fail with raw text;
//! - `stderr-exit <text>`: write stderr and exit without replying;
//! - `invalid-step`: reply with a malformed usage field carrying raw private text;
//! - `hang`: wait for `cancel`, then fail with kind `transient`, message `cancelled`;
//! - `exit`: exit without replying;
//! - `sleep <ms>`: reply after a delay;
//! - `probe`: reply `probe model=<model> tools=<comma-separated tool names>`;
//! - anything else: reply `ok`.
//!
//! With `BP_FAKE_REPORTED_MODEL=<id>` every reply, failures included, names that
//! full model ID as `reported_model`, as the Claude Code extension does for the SDK's model.
//!
//! With `BP_FAKE_FAIL=usage_limit` every step fails as a subscription usage
//! limit (`limit_type` `five_hour`); with `BP_FAKE_ENV` set, `provider_state.env`
//! echoes it and `BUBBLEPAW_EXTENSION_ENV_KEYS`. With `BP_FAKE_REPLY=<text>` every
//! step replies with that text (after `BP_FAKE_REPLY_DELAY_MS`, if set).
//! `BP_FAKE_REPLY_AS_RESULT=1` instead submits its JSON through `submit_result`,
//! using the JS wrapper when declared, so delegate fixtures can finish a report.
//!
//! With `BP_FAKE_STEP_LOG=<path>` each received step appends a JSON line
//! `{"id","pid","step_count","params"}` and each `cancel` appends `{"cancel":<id>,"pid":<pid>}`.
//!
//! Every reply's `provider_state` echoes the received model, session, cursor
//! and tools, plus the complete step request, received step count, process id
//! and sessions closed so far. `--capabilities=<JSON>` adds extension-wide
//! capabilities to the initialize response; they are absent by default.
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    io::{BufRead, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct State {
    cancelled: Mutex<HashSet<u64>>,
    closed: Mutex<Vec<String>>,
    out: Mutex<()>,
    steps_received: AtomicU64,
    provider_state: Option<Value>,
    effect_code: Option<String>,
    step_log: Option<String>,
}
impl State {
    fn log(&self, value: Value) {
        let Some(path) = &self.step_log else {
            return;
        };
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("fake step log");
        writeln!(log, "{value}").expect("write fake step log");
    }
    fn send(&self, value: Value) {
        let _lock = self.out.lock().unwrap();
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{value}");
        let _ = stdout.flush();
    }
    fn result(&self, id: &Value, result: Value) {
        self.send(json!({"jsonrpc":"2.0","id":id,"result":result}));
    }
    fn error(&self, id: &Value, message: &str, data: Value) {
        self.send(
            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":message,"data":data}}),
        );
    }
}

fn main() {
    let provider_state = std::env::args()
        .find_map(|argument| {
            argument
                .strip_prefix("--provider-state-file=")
                .map(str::to_owned)
        })
        .map(|path| std::fs::read_to_string(path).expect("fake provider state file"))
        .or_else(|| {
            std::env::args().find_map(|argument| {
                argument
                    .strip_prefix("--provider-state=")
                    .map(str::to_owned)
            })
        })
        .or_else(|| std::env::var("BP_FAKE_PROVIDER_STATE").ok())
        .map(|value| serde_json::from_str(&value).expect("valid fake provider state"));
    let state = Arc::new(State {
        provider_state,
        effect_code: std::env::var("BP_FAKE_EFFECT_CODE").ok(),
        step_log: std::env::var("BP_FAKE_STEP_LOG").ok(),
        ..Default::default()
    });
    let exit_marker = std::env::var("BP_FAKE_EXIT_MARKER").ok();
    let capabilities = std::env::args().find_map(|argument| {
        argument.strip_prefix("--capabilities=").map(|value| {
            serde_json::from_str::<Value>(value).expect("valid fake extension capabilities")
        })
    });
    eprintln!("fake extension started");
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = message["id"].clone();
        match message["method"].as_str().unwrap_or("") {
            "initialize" => {
                let mut result = json!({"provider":"fake","models":[
                    {"id":"code-model","name":"Code model","capabilities":{"context_window":200000,"cache":"session","tool_surface":{"kind":"code"},"stateful":true}},
                    {"id":"direct-model","name":"Direct model","capabilities":{"context_window":null,"cache":"none","tool_surface":{"kind":"direct"},"stateful":false}}
                ]});
                if let Some(capabilities) = &capabilities {
                    result["capabilities"] = capabilities.clone();
                }
                state.result(&id, result);
            }
            "cancel" => {
                if let Some(target) = message["params"]["id"].as_u64() {
                    state.cancelled.lock().unwrap().insert(target);
                    state.log(json!({"cancel":target,"pid":std::process::id()}));
                }
            }
            "session.close" => {
                if let Some(session) = message["params"]["session_id"].as_str() {
                    state.closed.lock().unwrap().push(session.into());
                }
                state.result(&id, json!({}));
            }
            "step" => {
                let step_count = state.steps_received.fetch_add(1, Ordering::SeqCst) + 1;
                let params = message["params"].clone();
                state.log(json!({"id":id,"pid":std::process::id(),"step_count":step_count,"params":params}));
                let state = state.clone();
                std::thread::spawn(move || step(&state, id, params, step_count));
            }
            _ => state.error(&id, "unknown method", json!({"kind":"fatal"})),
        }
    }
    if let Some(path) = exit_marker {
        std::fs::write(path, "exited").expect("fake exit marker");
    }
}

/// Adds the `reported_model` a Claude Code step reports, when the test asks for one.
fn with_reported_model(mut value: Value) -> Value {
    if let Ok(model) = std::env::var("BP_FAKE_REPORTED_MODEL") {
        value["reported_model"] = json!(model);
    }
    value
}

fn step(state: &State, id: Value, params: Value, step_count: u64) {
    let mut echo = state.provider_state.clone().unwrap_or_else(|| json!({
        "model": params["model"],
        "session_id": params["session_id"],
        "history": params["history"],
        "tools": params["tools"].as_array().map(|tools| tools.iter().map(|t| t["name"].clone()).collect::<Vec<_>>()),
        "pid": std::process::id(),
        "closed": *state.closed.lock().unwrap(),
        "request": params,
        "step_count": step_count,
    }));
    if let Ok(value) = std::env::var("BP_FAKE_ENV") {
        echo["env"] = json!({
            "BP_FAKE_ENV": value,
            "BUBBLEPAW_EXTENSION_ENV_KEYS": std::env::var("BUBBLEPAW_EXTENSION_ENV_KEYS").ok(),
        });
    }
    if std::env::var("BP_FAKE_FAIL").as_deref() == Ok("usage_limit") {
        return state.error(
            &id,
            "fake usage limit",
            json!({"kind":"rate_limit","resets_at":"2026-10-19T18:00:00Z","limit_type":"five_hour"}),
        );
    }
    let last = params["messages"]
        .as_array()
        .and_then(|messages| {
            messages.iter().rev().find(|message| {
                // Generated reminders and legacy notices are not the prompt.
                !message["content"].as_str().is_some_and(|text| {
                    text.starts_with("[host notice]") || text.starts_with("<harness ")
                })
            })
        })
        .cloned()
        .unwrap_or(Value::Null);
    let content = last["content"].as_str().unwrap_or("").to_string();
    let reply = |text: &str, tool_calls: Value| {
        state.result(
            &id,
            with_reported_model(json!({"text":text,"tool_calls":tool_calls,"usage":{"input_tokens":5,"output_tokens":2,"cache_read_tokens":3},"provider_state":echo})),
        );
    };
    if let Ok(text) = std::env::var("BP_FAKE_REPLY") {
        if let Some(ms) = std::env::var("BP_FAKE_REPLY_DELAY_MS")
            .ok()
            .and_then(|ms| ms.parse().ok())
        {
            std::thread::sleep(Duration::from_millis(ms));
        }
        if std::env::var("BP_FAKE_REPLY_AS_RESULT").as_deref() == Ok("1") {
            let result: Value = serde_json::from_str(&text).expect("fake structured reply JSON");
            let js = params["tools"]
                .as_array()
                .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == "js"));
            return reply(
                "",
                json!([{"id":format!("fake-result-{step_count}"),
                    "name":if js { "js" } else { "submit_result" },
                    "arguments":if js { json!({"code":format!("await tools.submit_result({result});")}) } else { result }}]),
            );
        }
        return reply(&text, json!([]));
    }
    if let Some(code) = &state.effect_code {
        return if step_count == 1 {
            reply(
                "",
                json!([{"id":"interrupted-extension-effect","name":"js","arguments":{"code":code}}]),
            )
        } else {
            reply("done", json!([]))
        };
    }
    if last["role"] == "tool" {
        return reply(&format!("tool result: {content}"), json!([]));
    }
    let (command, argument) = content.split_once(' ').unwrap_or((content.as_str(), ""));
    match command {
        "text" | "text-delay" | "text-error" => {
            let (delay, pieces) = if command == "text-delay" {
                let (ms, pieces) = argument.split_once(' ').unwrap_or(("0", ""));
                (ms.parse().unwrap_or(0), pieces)
            } else {
                (0, argument)
            };
            let pieces: Vec<&str> = pieces.split('|').collect();
            for piece in &pieces {
                state
                    .send(json!({"jsonrpc":"2.0","method":"text","params":{"id":id,"text":piece}}));
            }
            if command == "text-error" {
                return state.error(
                    &id,
                    "RAW_RPC_ERROR_SENTINEL after text",
                    json!({"kind":"transient"}),
                );
            }
            std::thread::sleep(Duration::from_millis(delay));
            reply(&pieces.concat(), json!([]));
        }
        "bad-call" => {
            let arguments = if argument == "null" {
                Value::Null
            } else {
                json!({"value":17})
            };
            reply(
                "",
                json!([{"id":"bad-extension-call","name":"echo","arguments":arguments}]),
            );
        }
        "call" | "js" => {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let call_id = format!("call-{}-{id}-{nanos}", std::process::id());
            reply(
                "",
                if command == "js" {
                    json!([{"id":call_id,"name":"js","arguments":{"code":argument}}])
                } else {
                    json!([{"id":call_id,"name":if argument.is_empty() { "echo" } else { argument },"arguments":{"value":"hi"}}])
                },
            );
        }
        "error" | "raw-error" | "option-error" => {
            let (kind, raw) = if command == "error" {
                (argument, String::new())
            } else {
                let (prefix, raw) = argument.split_once('|').unwrap_or((argument, ""));
                if command == "option-error" {
                    ("fatal", format!("{prefix}: {raw}"))
                } else {
                    (prefix, raw.to_owned())
                }
            };
            let data = if kind == "rate_limit" {
                json!({"kind":"rate_limit","resets_at":"2026-10-08T14:00:00Z"})
            } else {
                json!({"kind":kind})
            };
            let message = if command == "error" {
                format!("fake {kind}")
            } else {
                raw
            };
            state.error(&id, &message, with_reported_model(data));
        }
        "usage-error" => state.error(
            &id,
            &format!("fake {argument} after reported usage"),
            with_reported_model(json!({
                "kind": argument,
                "usage": {"input_tokens":5,"output_tokens":2,"cache_read_tokens":3}
            })),
        ),
        "hang" => {
            let target = id.as_u64().unwrap_or(0);
            loop {
                if state.cancelled.lock().unwrap().remove(&target) {
                    return state.error(&id, "cancelled", json!({"kind":"transient"}));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        "exit" => std::process::exit(7),
        "stderr-exit" => {
            eprintln!("{argument}");
            std::thread::sleep(Duration::from_millis(100));
            std::process::exit(7);
        }
        "invalid-step" => state.result(
            &id,
            json!({"text":"","tool_calls":[],"usage":{"input_tokens":"RAW_INVALID_STEP_SENTINEL","output_tokens":2}}),
        ),
        "probe" => {
            let tools = params["tools"]
                .as_array()
                .map(|tools| {
                    tools
                        .iter()
                        .filter_map(|tool| tool["name"].as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            reply(
                &format!(
                    "probe model={} tools={tools}",
                    params["model"].as_str().unwrap_or("")
                ),
                json!([]),
            );
        }
        "sleep" => {
            let ms = argument.parse().unwrap_or(100);
            std::thread::sleep(Duration::from_millis(ms));
            reply(&format!("slept {ms}"), json!([]));
        }
        _ => reply("ok", json!([])),
    }
}
