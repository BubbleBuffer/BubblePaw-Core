use super::*;

#[tokio::test]
async fn native_reload_and_external_inventory_changes_append_notice_without_changing_prefix()
-> Result<()> {
    for explicit in [true, false] {
        let f = fixture(
            ExecutionMode::Native,
            true,
            vec![
                call("reload", json!({})),
                call("tool_search", json!({"query":"late.server"})),
                call(
                    "call_tool",
                    json!({"name":"mcp__late__lookup","arguments":{"value":"fresh"}}),
                ),
                call(
                    "call_tool",
                    json!({"name":"new_core","arguments":{"value":"new core reached"}}),
                ),
                call("call_tool", json!({"name":"former_core","arguments":{}})),
                done(),
                done(),
            ],
            false,
            explicit,
        )?;
        assert_eq!(
            f.harness
                .prompt(&f.session, "reload", RunOptions::default())
                .await?
                .status,
            "completed"
        );
        {
            let requests = f.provider.requests.lock().unwrap();
            for request in requests.iter() {
                assert_eq!(
                    serde_json::to_vec(&request.tools)?,
                    serde_json::to_vec(&requests[0].tools)?
                );
                assert_eq!(request.system_prompt, requests[0].system_prompt);
                assert!(!request.system_prompt.contains("late.server"));
            }
            let notice = requests[1]
                .messages
                .iter()
                .find(|message| message.content.contains("Tool inventory changed"))
                .unwrap();
            // Runtime notices use generated user-role harness blocks for provider replay.
            assert_eq!(
                notice.origin,
                Some(bubblepaw_core::MessageOrigin::Generated)
            );
            assert!(notice.content.contains("<harness kind=\"tools_reloaded\">"));
            assert!(
                notice.content.contains("mcp__late__lookup")
                    && notice.content.contains("former_core")
            );
            assert!(notice.content.contains("tool_search") && notice.content.contains("call_tool"));
            let encoded = notice
                .content
                .lines()
                .find(|line| line.starts_with('{'))
                .unwrap();
            let diff: Value = serde_json::from_str(encoded)?;
            assert_eq!(serde_json::to_string(&diff)?, encoded);
            let added = diff["added"].as_array().unwrap();
            assert_eq!(
                added
                    .iter()
                    .find(|tool| tool["name"] == "new_core")
                    .unwrap(),
                &serde_json::to_value(Echo("new_core").definition())?
            );
            assert_eq!(
                added
                    .iter()
                    .find(|tool| tool["name"] == "mcp__late__lookup")
                    .unwrap(),
                &json!({"name":"mcp__late__lookup","server":"late.server","deferred":true})
            );
            assert!(
                !notice
                    .content
                    .contains("DEFERRED_SCHEMA_MUST_NOT_APPEAR_IN_NOTICE")
            );
            assert!(!notice.content.contains("deferred_only_detail"));
            assert_eq!(diff["removed"], json!(["former_core", "mcp__old__lookup"]));
            assert!(!notice.content.contains("Inspect former_core records"));
            assert!(!notice.content.contains("Inspect mcp__old__lookup records"));
            assert_eq!(
                added
                    .iter()
                    .map(|tool| tool["name"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                ["mcp__late__lookup", "new_core"]
            );
        }
        let messages = f.store.messages(&f.session)?;
        assert!(messages.iter().any(|message| message.role == Role::Tool
            && !message.is_error
            && message.content.contains("fresh")));
        assert!(messages.iter().any(|message| message.role == Role::Tool
            && !message.is_error
            && message.content.contains("new core reached")));
        assert!(messages.iter().any(|message| {
            message.is_error
                && message
                    .content
                    .contains("Tool former_core is unavailable: removed by reload")
        }));
        // The chat retains its initial declarations and anchors in later runs.
        assert_eq!(
            f.harness
                .prompt(&f.session, "next run", RunOptions::default())
                .await?
                .status,
            "completed"
        );
        let requests = f.provider.requests.lock().unwrap();
        let last = requests.last().unwrap();
        assert_eq!(frozen(last)?, frozen(&requests[0])?);
    }
    Ok(())
}

#[tokio::test]
async fn compaction_apply_folds_inventory_into_cold_catalog_without_changing_chat_prefix()
-> Result<()> {
    for mode in [ExecutionMode::Native, ExecutionMode::Js] {
        let f = fixture(
            mode,
            true,
            vec![
                routed(mode, "reload", json!({})),
                routed(mode, "new_core", json!({"value":"after reload"})),
                done(),
                done(),
            ],
            true,
            true,
        )?;
        for input in ["apply", "later turn"] {
            let run = f
                .harness
                .prompt(&f.session, input, RunOptions::default())
                .await?;
            assert_eq!(run.status, "completed", "{:?}", run.error);
        }
        let requests = f.provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(frozen(&requests[1])?, frozen(&requests[0])?);
        for request in &requests[2..] {
            assert_eq!(frozen(request)?, frozen(&requests[0])?);
            let (message, encoded) = request
                .messages
                .iter()
                .find_map(|message| {
                    let (_, tail) = message
                        .content
                        .split_once("<harness kind=\"tool_catalog\">")?;
                    Some((message, tail.split_once("</harness>")?.0))
                })
                .expect("applied inventory remains in the cold context");
            assert_eq!(message.role, Role::System);
            let catalog: Value = serde_json::from_str(encoded)?;
            assert_eq!(
                serde_json::to_string(&catalog)?,
                encoded,
                "catalog is compact JSON"
            );
            assert!(catalog["anchors"].as_str().unwrap().contains("late.server"));
            assert!(
                !catalog["anchors"]
                    .as_str()
                    .unwrap()
                    .contains("original.server")
            );
            let added = catalog["added"].as_array().unwrap();
            let core = added
                .iter()
                .find(|tool| tool["name"] == "new_core")
                .unwrap();
            assert_eq!(core, &serde_json::to_value(Echo("new_core").definition())?);
            assert!(added.iter().any(|tool| tool["name"] == "mcp__late__lookup"
                && tool["server"] == "late.server"
                && tool["deferred"] == true));
            assert!(!encoded.contains("DEFERRED_SCHEMA_MUST_NOT_APPEAR_IN_NOTICE"));
            assert!(!encoded.contains("deferred_only_detail"));
            assert!(
                catalog["removed"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("former_core"))
            );
            assert!(
                catalog["removed"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("mcp__old__lookup"))
            );
            assert!(
                !request
                    .messages
                    .iter()
                    .any(|message| message.content.contains("Tool inventory changed")),
                "the applied view can fold the old live notice without losing current tools"
            );
            assert!(request.cache_tiers.unwrap().cold_end.is_some());
        }
    }
    Ok(())
}
