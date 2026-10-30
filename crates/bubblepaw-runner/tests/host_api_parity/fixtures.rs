//! Fixture vocabulary. A fixture names the table method it proves and the calls
//! that exercise it. In params, `$role` becomes the transport's label and
//! `$name` becomes a value bound by an earlier step in the same fixture.
use super::{fixtures_runs, fixtures_services, fixtures_sessions};
use serde_json::{Value, json};
use std::collections::HashMap;

pub struct Fixture {
    pub method: &'static str,
    pub steps: Vec<Step>,
}

#[derive(Clone)]
pub struct Step {
    pub method: &'static str,
    pub params: Value,
    pub expect: Expect,
    /// `(name, JSON pointer into the result)` stored for later steps.
    pub bind: Option<(&'static str, &'static str)>,
    pub settle: Settle,
    pub view: View,
    /// Substrings the raw reply must contain (e.g. user text that must survive).
    pub contains: Vec<&'static str>,
}

#[derive(Clone, Copy)]
pub enum Expect {
    Ok,
    /// Must fail, and the message must contain this text.
    Error(&'static str),
    /// Must fail; the exact wording is not pinned by this fixture.
    AnyError,
}

/// What to wait for after a step.
#[derive(Clone, Copy)]
pub enum Settle {
    None,
    /// The named session has no running run, queued input or pending question.
    Idle(&'static str),
    /// The named session shows a pending question; its id is bound as `question`.
    Question(&'static str),
}

#[derive(Clone, Copy)]
pub enum View {
    Full,
    /// Only this transport's session rows (global session lists).
    Sessions,
    /// Structure only, for store-wide counters.
    Shape,
}

pub fn fixture(method: &'static str, steps: Vec<Step>) -> Fixture {
    Fixture { method, steps }
}

pub fn ok(method: &'static str, params: Value) -> Step {
    Step {
        method,
        params,
        expect: Expect::Ok,
        bind: None,
        settle: Settle::None,
        view: View::Full,
        contains: Vec::new(),
    }
}

pub fn err(method: &'static str, params: Value, message: &'static str) -> Step {
    Step {
        expect: Expect::Error(message),
        ..ok(method, params)
    }
}

pub fn any_err(method: &'static str, params: Value) -> Step {
    Step {
        expect: Expect::AnyError,
        ..ok(method, params)
    }
}

impl Step {
    pub fn bind(mut self, name: &'static str, pointer: &'static str) -> Self {
        self.bind = Some((name, pointer));
        self
    }
    pub fn idle(mut self, session: &'static str) -> Self {
        self.settle = Settle::Idle(session);
        self
    }
    pub fn question(mut self, session: &'static str) -> Self {
        self.settle = Settle::Question(session);
        self
    }
    pub fn sessions(mut self) -> Self {
        self.view = View::Sessions;
        self
    }
    pub fn shape(mut self) -> Self {
        self.view = View::Shape;
        self
    }
    pub fn contains(mut self, text: &'static str) -> Self {
        self.contains.push(text);
        self
    }
}

/// Create a paired target: a session titled for this fixture and transport.
pub fn open(label: &'static str, bind: &'static str) -> Step {
    ok(
        "session.open",
        json!({"title": format!("parity-$role-{label}")}),
    )
    .bind(bind, "/id")
}

/// Create a child of the session bound to `parent`.
pub fn open_child(label: &'static str, parent: &'static str, bind: &'static str) -> Step {
    ok(
        "session.open",
        json!({"title": format!("parity-$role-{label}"), "parent_session_id": format!("${parent}")}),
    )
    .bind(bind, "/id")
}

/// An ordinary run with scripted, offline output; waits for the run to finish.
pub fn run_text(session: &'static str, text: &str) -> Step {
    ok(
        "run.start",
        json!({"session_id": format!("${session}"), "text": text}),
    )
    .idle(session)
}

/// A run whose script asks a question and stays pending until answered.
pub fn question_run(session: &'static str) -> Step {
    ok(
        "run.start",
        json!({
            "session_id": format!("${session}"),
            "text": "parity question",
            "options": {"script": "return await tools.question({text:'Which colour?',options:['Blue','Green']});"},
        }),
    )
    .question(session)
}

/// A run whose script records a todo item, then finishes.
pub fn todo_run(session: &'static str) -> Step {
    ok(
        "run.start",
        json!({
            "session_id": format!("${session}"),
            "text": "parity todo",
            "options": {"script": "await tools.todo({items:[{text:'parity todo',status:'in_progress'}]}); return 'ok';"},
        }),
    )
    .idle(session)
}

pub fn all() -> Vec<Fixture> {
    let mut all = fixtures_sessions::all();
    all.extend(fixtures_runs::all());
    all.extend(fixtures_services::all());
    // Shutdown ends each transport's process, so it runs after everything else.
    all.push(fixture(
        "shutdown",
        vec![ok("shutdown", json!({})).contains("stopped")],
    ));
    all
}

/// Substitute placeholders. A `$name` that no earlier step bound is a fixture
/// bug and fails loudly rather than reaching the transport.
pub fn resolve(value: &Value, role: &str, vars: &HashMap<&'static str, Value>) -> Value {
    match value {
        Value::String(text) => {
            if let Some(name) = text.strip_prefix('$').filter(|name| {
                !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            }) && name != "role"
            {
                return vars
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| panic!("fixture variable ${name} is not bound"));
            }
            Value::String(text.replace("$role", role))
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| resolve(item, role, vars)).collect())
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), resolve(item, role, vars)))
                .collect(),
        ),
        other => other.clone(),
    }
}
