//! Scripted provider for the idle-completion notice test, and a chat store size probe.
use bubblepaw_core::CancellationToken;
use serde_json::json;
use std::path::Path;

pub struct NoticeProvider {
    pub code: String,
    pub requests: std::sync::Mutex<Vec<Vec<String>>>,
}
pub fn chat_measure(path: &Path) -> (u64, u64, u64, u64) {
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let rows = |table: &str| {
        connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|count| u64::try_from(count).unwrap())
            .unwrap()
    };
    let bytes = |path: &Path| {
        std::fs::metadata(path)
            .map(|metadata| metadata.len())
            .unwrap_or(0)
    };
    (
        rows("messages"),
        rows("effects"),
        bytes(path),
        bytes(Path::new(&format!("{}-wal", path.display()))),
    )
}
#[async_trait::async_trait]
impl bubblepaw_core::Provider for NoticeProvider {
    async fn generate(
        &self,
        request: bubblepaw_core::ModelRequest,
        _: bubblepaw_core::TextCallback,
        _: CancellationToken,
    ) -> anyhow::Result<bubblepaw_core::ModelResponse> {
        let mut requests = self.requests.lock().unwrap();
        let first = requests.is_empty();
        requests.push(
            request
                .messages
                .iter()
                .map(|message| message.content.clone())
                .collect(),
        );
        Ok(if first {
            bubblepaw_core::ModelResponse {
                tool_calls: vec![bubblepaw_core::ToolCall {
                    id: "start-job".into(),
                    name: "js".into(),
                    arguments: json!({"code":self.code}),
                }],
                ..Default::default()
            }
        } else {
            bubblepaw_core::ModelResponse {
                text: "idle".into(),
                ..Default::default()
            }
        })
    }
}
