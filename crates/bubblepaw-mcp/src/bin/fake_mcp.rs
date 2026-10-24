//! Scripted MCP fixture, never a live model/server acceptance claim.
//! Flags: --log PATH (JSONL requests with pid), --pid-file PATH, --version V,
//! --initialize-delay-ms N, --list-delay-ms N, --invalid-schema, --collision,
//! --paginate, --repeat-cursor, --malformed-initialize, --exit-initialize,
//! --stderr-env NAME, --child-pid-file PATH (Unix), --ignore-eof.
//! Tools: echo {text}, fail/error, rpc_error, slow {ms}, crash, pid, cancelled,
//! structured, image/binary, env {name}, cwd. Request handlers run concurrently.
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs::{File, OpenOptions},
    io::{BufRead, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

struct State {
    out: Mutex<()>,
    log: Mutex<Option<File>>,
    cancelled: Mutex<BTreeSet<u64>>,
    options: Options,
}
#[derive(Default)]
struct Options {
    log: Option<PathBuf>,
    tools_file: Option<PathBuf>,
    pid_file: Option<PathBuf>,
    child_pid_file: Option<PathBuf>,
    version: String,
    initialize_delay_ms: u64,
    list_delay_ms: u64,
    invalid_schema: bool,
    collision: bool,
    paginate: bool,
    repeat_cursor: bool,
    malformed_initialize: bool,
    exit_initialize: bool,
    stderr_env: Option<String>,
    ignore_eof: bool,
}
impl State {
    fn send(&self, value: Value) {
        let _guard = self.out.lock().unwrap();
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{value}");
        let _ = stdout.flush();
    }
    fn result(&self, id: &Value, result: Value) {
        self.send(json!({"jsonrpc":"2.0","id":id,"result":result}));
    }
    fn error(&self, id: &Value, message: &str) {
        self.send(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":message}}));
    }
}

fn options() -> Options {
    let mut options = Options {
        version: "2025-06-18".into(),
        ..Default::default()
    };
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--log" => options.log = args.next().map(Into::into),
            "--tools-file" => options.tools_file = args.next().map(Into::into),
            "--pid-file" => options.pid_file = args.next().map(Into::into),
            "--child-pid-file" => options.child_pid_file = args.next().map(Into::into),
            "--version" => options.version = args.next().unwrap_or_default(),
            "--initialize-delay-ms" => {
                options.initialize_delay_ms = args.next().and_then(|a| a.parse().ok()).unwrap_or(0)
            }
            "--list-delay-ms" => {
                options.list_delay_ms = args.next().and_then(|a| a.parse().ok()).unwrap_or(0)
            }
            "--invalid-schema" => options.invalid_schema = true,
            "--collision" => options.collision = true,
            "--paginate" => options.paginate = true,
            "--repeat-cursor" => options.repeat_cursor = true,
            "--malformed-initialize" => options.malformed_initialize = true,
            "--exit-initialize" => options.exit_initialize = true,
            "--stderr-env" => options.stderr_env = args.next(),
            "--ignore-eof" => options.ignore_eof = true,
            _ => {
                eprintln!("unknown fake MCP option");
                std::process::exit(2);
            }
        }
    }
    options
}

fn main() {
    let options = options();
    if let Some(path) = &options.pid_file {
        std::fs::write(path, std::process::id().to_string()).unwrap();
    }
    #[cfg(unix)]
    if let Some(path) = &options.child_pid_file {
        // This grandchild deliberately holds a pipe and ignores the parent
        // lifetime; the host's Unix process group owns its termination.
        let mut child = std::process::Command::new("sleep")
            .arg("300")
            .spawn()
            .unwrap();
        std::fs::write(path, child.id().to_string()).unwrap();
        // Reap it if this fixture remains running. Exiting the fixture does
        // not join the thread, so host acceptance still checks tree cleanup.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
    eprintln!("fake MCP started");
    if let Some(name) = &options.stderr_env {
        eprintln!("stderr value: {}", std::env::var(name).unwrap_or_default());
    }
    let log = options.log.as_ref().map(|path| {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap()
    });
    let state = Arc::new(State {
        out: Mutex::default(),
        log: Mutex::new(log),
        cancelled: Mutex::default(),
        options,
    });
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else {
            break;
        };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(log) = state.log.lock().unwrap().as_mut() {
            let _ = writeln!(
                log,
                "{}",
                json!({"pid":std::process::id(),"request":message})
            );
            let _ = log.flush();
        }
        let id = message["id"].clone();
        match message["method"].as_str().unwrap_or("") {
            "initialize" => {
                if state.options.exit_initialize {
                    std::process::exit(8);
                }
                if state.options.malformed_initialize {
                    println!("this is not JSON-RPC");
                    std::io::stdout().flush().unwrap();
                    continue;
                }
                std::thread::sleep(Duration::from_millis(state.options.initialize_delay_ms));
                state.result(&id, json!({"protocolVersion":state.options.version,"capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"1"}}));
            }
            "notifications/initialized" => {}
            "notifications/cancelled" => {
                if let Some(id) = message["params"]["requestId"].as_u64() {
                    state.cancelled.lock().unwrap().insert(id);
                }
            }
            "tools/list" => {
                std::thread::sleep(Duration::from_millis(state.options.list_delay_ms));
                let mut tools: Vec<Value> = ["echo", "fail", "error", "rpc_error", "slow", "crash", "pid", "cancelled", "structured", "image", "binary", "env", "cwd"].iter()
                    .map(|name| json!({"name":name,"description":format!("fake {name}"),"inputSchema":{"type":"object"},"annotations":{"readOnlyHint":true}})).collect();
                if let Some(path) = &state.options.tools_file {
                    tools = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
                }
                if state.options.invalid_schema {
                    tools.push(json!({"name":"bad_schema","inputSchema":{"type":17}}));
                }
                if state.options.collision {
                    tools.push(json!({"name":"a.b","inputSchema":{"type":"object"}}));
                    tools.push(json!({"name":"a_b","inputSchema":{"type":"object"}}));
                }
                if state.options.repeat_cursor {
                    state.result(&id, json!({"tools":[],"nextCursor":"repeat"}));
                } else if state.options.paginate {
                    if message["params"]["cursor"] == "second" {
                        state.result(&id, json!({"tools":tools.split_off(1)}));
                    } else {
                        state.result(&id, json!({"tools":[tools[0]],"nextCursor":"second"}));
                    }
                } else {
                    state.result(&id, json!({"tools":tools}));
                }
            }
            "tools/call" => {
                let state = state.clone();
                let params = message["params"].clone();
                std::thread::spawn(move || call(&state, &id, params));
            }
            _ => state.error(&id, "unknown method"),
        }
    }
    if state.options.ignore_eof {
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }
}

fn call(state: &State, id: &Value, params: Value) {
    let input = &params["arguments"];
    let text = match params["name"].as_str().unwrap_or("") {
        "echo" => {
            if input["notify_tools_changed"] == true {
                state.send(json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"}));
            }
            input["text"].as_str().unwrap_or("").to_owned()
        }
        "fail" | "error" => {
            state.result(
                id,
                json!({"content":[{"type":"text","text":"fake tool error"}],"isError":true}),
            );
            return;
        }
        "rpc_error" => {
            state.error(id, "fake RPC failure");
            return;
        }
        "slow" => {
            let target = id.as_u64().unwrap_or(0);
            let ms = input["ms"].as_u64().unwrap_or(5000);
            let start = std::time::Instant::now();
            while start.elapsed() < Duration::from_millis(ms) {
                if state.cancelled.lock().unwrap().contains(&target) {
                    state.error(id, "cancelled");
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            format!("slept {ms}")
        }
        "crash" => std::process::exit(7),
        "pid" => std::process::id().to_string(),
        "cancelled" => serde_json::to_string(&*state.cancelled.lock().unwrap()).unwrap(),
        "structured" => {
            state.result(id, json!({"content":[],"structuredContent":{"answer":42}}));
            return;
        }
        "binary" => {
            state.result(id, json!({"content":[{"type":"image","data":"YWJjZA==","mimeType":"image/png"},{"type":"audio","data":"YQ==","mimeType":"audio/wav"},{"type":"resource","resource":{"uri":"test://fixture","text":"embedded"}}]}));
            return;
        }
        "image" => {
            state.result(
                id,
                json!({"content":[{"type":"image","data":"YWJjZA==","mimeType":"image/png"}]}),
            );
            return;
        }
        "env" => std::env::var(input["name"].as_str().unwrap_or("")).unwrap_or_default(),
        "cwd" => std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        _ => {
            state.error(id, "unknown tool");
            return;
        }
    };
    state.result(id, json!({"content":[{"type":"text","text":text}]}));
}
