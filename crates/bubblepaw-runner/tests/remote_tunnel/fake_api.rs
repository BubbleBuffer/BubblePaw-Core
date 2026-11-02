//! A local stand-in for the few Cloudflare API v4 calls domain setup makes.
//! It knows one zone, `example.com` in account `acct-1`, and wants the bearer
//! token [`TOKEN`].
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub const TOKEN: &str = "test-token-not-real";
pub const TUNNEL_ID: &str = "0f9e8d7c-6b5a-4433-8211-00aabbccddee";

#[derive(Default)]
pub struct Recorded {
    /// The `tunnel_secret` of every tunnel created.
    pub secrets: Vec<String>,
    pub records: Vec<Value>,
}

pub struct FakeApi {
    pub url: String,
    pub state: Arc<Mutex<Recorded>>,
    _task: tokio::task::JoinHandle<()>,
}

type Shared = State<Arc<Mutex<Recorded>>>;
type Reply = (StatusCode, Json<Value>);

fn ok(result: Value) -> Reply {
    (
        StatusCode::OK,
        Json(json!({"success": true, "errors": [], "result": result})),
    )
}

fn refused(headers: &HeaderMap) -> Option<Reply> {
    let expected = format!("Bearer {TOKEN}");
    let auth = headers.get("authorization").and_then(|v| v.to_str().ok());
    (auth != Some(expected.as_str())).then(|| {
        (
            StatusCode::FORBIDDEN,
            Json(json!({"success": false, "errors": [{"code": 10000, "message": "Authentication error"}]})),
        )
    })
}

async fn zones(headers: HeaderMap, Query(query): Query<HashMap<String, String>>) -> Reply {
    if let Some(refused) = refused(&headers) {
        return refused;
    }
    let found = match query.get("name").map(String::as_str) {
        Some("example.com") => {
            json!([{"id": "zone-1", "name": "example.com", "account": {"id": "acct-1"}}])
        }
        _ => json!([]),
    };
    ok(found)
}

async fn create_tunnel(
    State(state): Shared,
    headers: HeaderMap,
    Path(account): Path<String>,
    Json(body): Json<Value>,
) -> Reply {
    if let Some(refused) = refused(&headers) {
        return refused;
    }
    assert_eq!(account, "acct-1");
    assert_eq!(body["config_src"], "local");
    let secret = body["tunnel_secret"].as_str().unwrap().to_owned();
    state.lock().unwrap().secrets.push(secret);
    ok(json!({"id": TUNNEL_ID, "name": body["name"]}))
}

async fn list_records(
    State(state): Shared,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Reply {
    if let Some(refused) = refused(&headers) {
        return refused;
    }
    let name = query.get("name").cloned().unwrap_or_default();
    let records: Vec<Value> = state
        .lock()
        .unwrap()
        .records
        .iter()
        .filter(|record| record["name"] == name.as_str())
        .cloned()
        .collect();
    ok(json!(records))
}

async fn add_record(
    State(state): Shared,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> Reply {
    if let Some(refused) = refused(&headers) {
        return refused;
    }
    let mut state = state.lock().unwrap();
    body["id"] = json!(format!("record-{}", state.records.len() + 1));
    state.records.push(body.clone());
    ok(body)
}

async fn replace_record(
    State(state): Shared,
    headers: HeaderMap,
    Path((_zone, id)): Path<(String, String)>,
    Json(mut body): Json<Value>,
) -> Reply {
    if let Some(refused) = refused(&headers) {
        return refused;
    }
    body["id"] = json!(id);
    let mut state = state.lock().unwrap();
    for record in &mut state.records {
        if record["id"] == id.as_str() {
            *record = body.clone();
        }
    }
    ok(body)
}

impl FakeApi {
    pub async fn start(records: Vec<Value>) -> Self {
        let state = Arc::new(Mutex::new(Recorded {
            records,
            ..Recorded::default()
        }));
        let app = Router::new()
            .route("/zones", get(zones))
            .route("/accounts/{account}/cfd_tunnel", post(create_tunnel))
            .route(
                "/zones/{zone}/dns_records",
                get(list_records).post(add_record),
            )
            .route("/zones/{zone}/dns_records/{id}", put(replace_record))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            url,
            state,
            _task: task,
        }
    }
}
