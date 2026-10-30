//! JSON-RPC envelope handling shared by stdio and HTTP RPC.
use super::*;
fn failure(id: Value, code: i32, message: impl std::fmt::Display) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message.to_string()}})
}
pub async fn handle_bytes(ctx: &HostCtx, profile: Profile, bytes: &[u8]) -> Option<Value> {
    match serde_json::from_slice(bytes) {
        Ok(request) => handle(ctx, profile, request).await,
        Err(_) => Some(failure(Value::Null, -32700, "invalid JSON")),
    }
}
pub async fn handle(ctx: &HostCtx, profile: Profile, request: Value) -> Option<Value> {
    let id = request.get("id").cloned();
    if request["jsonrpc"] != "2.0"
        || !request["method"].is_string()
        || id
            .as_ref()
            .is_some_and(|id| !id.is_string() && !id.is_number() && !id.is_null())
    {
        return Some(failure(Value::Null, -32600, "invalid request"));
    }
    let method = request["method"].as_str().expect("validated method");
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = dispatch(ctx, profile, method, params).await;
    id.map(|id| match result {
        Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
        Err(error) => failure(id, error.rpc_code(), error),
    })
}
