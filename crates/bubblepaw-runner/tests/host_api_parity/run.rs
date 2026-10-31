//! Runs the fixtures through one transport and records each normalized reply.
//! The comparison then demands that the two recordings match line for line.
use super::{
    fixtures::{Expect, Fixture, Settle, Step, View, resolve},
    normalize,
    peer::{Peer, Transport},
};
use serde_json::{Value, json};
use std::{collections::HashMap, path::Path, time::Duration};
use tokio::time::Instant;

/// How long a session may take to go idle or to show its question.
const SETTLE: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(25);

/// One step as a transport answered it, after normalization.
#[derive(Debug)]
pub struct Recorded {
    pub label: String,
    pub reply: Value,
}

/// Run every fixture, in order, against one transport's process. The last
/// fixture is `shutdown`, so the process must exit on its own afterwards.
pub async fn phase(
    transport: Transport,
    root: &Path,
    role: &str,
    fixtures: &[Fixture],
) -> Vec<Recorded> {
    println!("PHASE {transport:?} start");
    let mut peer = Peer::start(transport, root).await;
    let mut recorded = Vec::new();
    for fixture in fixtures {
        let mut vars: HashMap<&'static str, Value> = HashMap::new();
        for (index, step) in fixture.steps.iter().enumerate() {
            let label = format!("{}#{index}", fixture.method);
            println!("STEP {transport:?} {label}");
            let reply = run_step(&mut peer, step, role, &mut vars, &label).await;
            recorded.push(Recorded {
                reply: normalized(&reply, step.view, role),
                label,
            });
        }
    }
    peer.finish().await;
    recorded
}

/// Both recordings must cover the same steps with the same normalized replies.
pub fn compare(stdio: &[Recorded], rpc: &[Recorded]) {
    assert_eq!(
        stdio.len(),
        rpc.len(),
        "stdio and /rpc recorded different step counts"
    );
    for (left, right) in stdio.iter().zip(rpc) {
        assert_eq!(left.label, right.label, "step order differs");
        assert!(
            left.reply == right.reply,
            "{}: stdio and /rpc disagree\nstdio: {}\nrpc:   {}",
            left.label,
            left.reply,
            right.reply
        );
    }
}

async fn run_step(
    peer: &mut Peer,
    step: &Step,
    role: &str,
    vars: &mut HashMap<&'static str, Value>,
    label: &str,
) -> Value {
    let params = resolve(&step.params, role, vars);
    let reply = peer.call_with_id(json!(label), step.method, params).await;
    check(step, &reply, label);
    if let Some((name, pointer)) = step.bind {
        let value = reply["result"]
            .pointer(pointer)
            .unwrap_or_else(|| panic!("{label}: no {pointer} in {reply}"))
            .clone();
        vars.insert(name, value);
    }
    match step.settle {
        Settle::None => {}
        Settle::Idle(session) => {
            let id = session_id(vars, session, label);
            settle(peer, &id, label, step.settle, vars).await;
        }
        Settle::Question(session) => {
            let id = session_id(vars, session, label);
            settle(peer, &id, label, step.settle, vars).await;
        }
    }
    reply
}

fn session_id(vars: &HashMap<&'static str, Value>, name: &str, label: &str) -> String {
    vars.get(name)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("{label}: session ${name} is not bound"))
        .to_string()
}

/// Poll `session.state` until the wanted condition holds. Idle means no run is
/// running or waiting for quota, no input is still pending and no question is
/// pending. Consumed input records and queued reminders stay listed after a run,
/// so they do not count (see `awaits_run`).
/// A question wait binds the pending question's id as `question`.
async fn settle(
    peer: &mut Peer,
    session: &str,
    label: &str,
    settle: Settle,
    vars: &mut HashMap<&'static str, Value>,
) {
    let deadline = Instant::now() + SETTLE;
    loop {
        let state = peer
            .call("session.state", json!({"session_id": session}))
            .await;
        assert!(
            state.get("error").is_none(),
            "{label}: session.state failed: {state}"
        );
        let result = &state["result"];
        // A failed run leaves the session idle, so without this check a script
        // step would pass with an empty follow-up read.
        if let Some(run) = failed_run(result) {
            panic!("{label}: run failed while session {session} settled: {run}");
        }
        match settle {
            Settle::None => return,
            Settle::Idle(_) => {
                if idle(result) {
                    return;
                }
            }
            Settle::Question(_) => {
                if let Some(question) = result.get("question").filter(|q| !q.is_null()) {
                    let id = question
                        .get("id")
                        .unwrap_or_else(|| {
                            panic!("{label}: pending question has no id: {question}")
                        })
                        .clone();
                    vars.insert("question", id);
                    return;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "{label}: session {session} did not settle: {result}"
        );
        tokio::time::sleep(POLL).await;
    }
}

/// The latest run, when it failed. Its `error` is what the step needs to show.
fn failed_run(state: &Value) -> Option<&Value> {
    state["runs"]
        .as_array()?
        .last()
        .filter(|run| run["status"].as_str() == Some("failed"))
}

/// Input that a run still has to take. Plain `pending` input does: a followup or
/// a typed steer waits for a run to consume it. A generated steer in `pending`
/// is a queued reminder waiting for the next reminder flush, which only happens
/// during a run, so it does not block. `reminder_pending` is the same kind of
/// queued reminder once staged, and `consumed` is terminal. Both stay listed.
fn awaits_run(input: &Value) -> bool {
    let generated_steer =
        input["kind"].as_str() == Some("steer") && input["origin"].as_str() == Some("generated");
    input["state"].as_str() == Some("pending") && !generated_steer
}

fn idle(state: &Value) -> bool {
    let no_pending_input = state["inputs"]
        .as_array()
        .is_none_or(|inputs| !inputs.iter().any(awaits_run));
    let no_question = state["question"].is_null();
    let no_active_run = state["runs"].as_array().is_none_or(|runs| {
        runs.iter().all(|run| {
            !matches!(
                run["status"].as_str(),
                Some("running" | "waiting_for_quota")
            )
        })
    });
    no_pending_input && no_question && no_active_run
}

/// A reply must meet the step's expectation, and show every required text in
/// its raw form. A reply that fails either check stops the run at that step.
fn check(step: &Step, reply: &Value, label: &str) {
    match step.expect {
        Expect::Ok => assert!(
            reply.get("error").is_none() && reply.get("result").is_some(),
            "{label}: expected a result, got {reply}"
        ),
        Expect::Error(message) => {
            let got = reply["error"]["message"].as_str().unwrap_or_default();
            assert!(
                got.contains(message),
                "{label}: expected an error containing {message:?}, got {reply}"
            );
        }
        Expect::AnyError => assert!(
            reply.get("error").is_some(),
            "{label}: expected an error, got {reply}"
        ),
    }
    let raw = reply.to_string();
    for text in &step.contains {
        assert!(
            raw.contains(*text),
            "{label}: reply lacks {text:?}: {reply}"
        );
    }
}

/// The reply as compared: generated values masked, and the result narrowed to
/// this transport's own rows or to its structure when the view says so.
fn normalized(reply: &Value, view: View, role: &str) -> Value {
    let mut shown = reply.clone();
    if let Some(result) = reply.get("result") {
        shown["result"] = match view {
            View::Full => result.clone(),
            View::Sessions => normalize::role_sessions(result, role),
            View::Shape => normalize::shape(result),
        };
    }
    normalize::response(&shown, role)
}
