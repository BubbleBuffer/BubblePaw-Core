//! Chapter metadata compatibility keeps the existing test coverage unchanged.
use super::*;

#[tokio::test]
async fn escaped_output_is_parseable_and_oversized_structures_fail_clearly() {
    let fixture = Fixture::new();
    for _ in 0..55 {
        fixture.message(&format!("needle{}", "\u{0000}".repeat(4500)));
    }
    let page = fixture
        .call("history_read", json!({"from":0,"count":50}))
        .await;
    let entries = page["messages"].as_array().unwrap();
    assert!(!entries.is_empty() && entries.len() < 50);
    assert_eq!(page["next"], entries.len());
    assert!(entries.iter().all(|entry| entry["truncated"] == true));
    let matches = fixture
        .call("history_search", json!({"query":"needle","limit":50}))
        .await;
    assert!(!matches["matches"].as_array().unwrap().is_empty());
    assert_eq!(matches["more"], true);
    let oversized = Fixture::new();
    oversized.message("valid preceding record");
    oversized.insert(
        &oversized.session,
        Role::Assistant,
        "".into(),
        vec![ToolCall {
            id: "huge".into(),
            name: "saved".into(),
            arguments: json!({"payload":"x".repeat(70_000)}),
        }],
        None,
    );
    let prefix = oversized.call("history_read", json!({"from":0})).await;
    assert_eq!(prefix["messages"].as_array().unwrap().len(), 1);
    assert_eq!(prefix["next"], 1);
    let error = oversized
        .registry
        .get_host("history_read")
        .unwrap()
        .invoke(oversized.context(&oversized.session), json!({"from":1}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("message 1 tool calls"));
    let index = Fixture::new();
    for ordinal in 1..=130 {
        let message = index.message("range");
        index.chapter(ordinal, &message, &message, &"\u{0001}".repeat(80));
    }
    let error = index
        .registry
        .get_host("history_chapters")
        .unwrap()
        .invoke(index.context(&index.session), json!({}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("complete chapter index"));
}
