use super::*;

fn surface() -> ChatToolSurface {
    ChatToolSurface {
        system_prompt: "base; memory v1; anchors".into(),
        declarations: vec![ToolDefinition {
            name: "read".into(),
            description: "Read files".into(),
            input_schema: serde_json::json!({"type":"object"}),
            read_only: true,
            approval: crate::Approval::default(),
        }],
        initial_result: None,
        result_contract_changed: false,
        initial_catalog: Vec::new(),
        observed_catalog: Vec::new(),
        inventory_notice: None,
        inventory_notice_base: None,
        cold_catalog: Some("existing cold catalog".into()),
    }
}

fn view(session: &str) -> ContextView {
    ContextView {
        session_id: session.into(),
        frontier: None,
        system_prompt: String::new(),
        prefix: Vec::new(),
        masked_tool_output_ids: Vec::new(),
        clipped_tool_call_ids: Vec::new(),
        created_at: now(),
        threshold: 1000,
        budget: 500,
        tokens: 10,
        blocks: Vec::new(),
        blocks_estimated: true,
    }
}

#[test]
fn surface_rebind_keeps_original_and_tools_across_reopen() -> Result<()> {
    for central in [false, true] {
        let dir = tempfile::tempdir()?;
        let open = || {
            if central {
                Store::open_data_dir(dir.path().join("data"))
            } else {
                Store::open(dir.path().join("chat.db"))
            }
        };
        let store = open()?;
        let session = store.create_session("surface")?.id;
        let original = surface();
        store.bind_chat_tool_surface(&session, &original)?;
        let snapshot_key = format!("test_surface:{session}");
        let rebind = ContextSurfaceRebind {
            expected_system_prompt: original.system_prompt.clone(),
            system_prompt: "base; memory v2; anchors".into(),
            metadata: vec![(snapshot_key.clone(), "snapshot v2".into())],
        };
        store.save_context_view_with_surface(&view(&session), None, Some(&rebind))?;
        let mut expected = original.clone();
        expected.system_prompt.clone_from(&rebind.system_prompt);
        assert_eq!(
            serde_json::to_value(store.chat_tool_surface(&session)?.unwrap())?,
            serde_json::to_value(&expected)?
        );
        let mut next = rebind.clone();
        next.expected_system_prompt
            .clone_from(&rebind.system_prompt);
        next.system_prompt = "base; memory v3; anchors".into();
        next.metadata[0].1 = "snapshot v3".into();
        store.save_context_view_with_surface(&view(&session), None, Some(&next))?;
        store.flush_serving()?;
        drop(store);
        let reopened = open()?;
        let saved = reopened.chat_tool_surface(&session)?.unwrap();
        expected.system_prompt.clone_from(&next.system_prompt);
        assert_eq!(
            serde_json::to_value(saved)?,
            serde_json::to_value(expected)?
        );
        assert_eq!(
            reopened.chat_metadata(&session, &format!("original_tool_surface:{session}"))?,
            Some(serde_json::to_string(&original)?)
        );
        assert_eq!(
            reopened.chat_metadata(&session, &snapshot_key)?.as_deref(),
            Some("snapshot v3")
        );
    }
    Ok(())
}

#[test]
fn failed_surface_commit_restores_view_prompt_and_host_snapshot() -> Result<()> {
    for central in [false, true] {
        let dir = tempfile::tempdir()?;
        let store = if central {
            Store::open_data_dir(dir.path())?
        } else {
            Store::in_memory()?
        };
        let session = store.create_session("rollback")?.id;
        let original = surface();
        store.bind_chat_tool_surface(&session, &original)?;
        let previous = view(&session);
        store.save_context_view(&previous)?;
        store.flush_serving()?;
        let canonical = store.scoped(&session)?.unwrap_or_else(|| store.clone());
        canonical.read(|conn| {
            conn.execute_batch(
                "CREATE TABLE surface_commit_guard(parent TEXT REFERENCES sessions(id) DEFERRABLE INITIALLY DEFERRED);
                 CREATE TRIGGER reject_surface_commit AFTER INSERT ON metadata WHEN NEW.value='reject snapshot'
                 BEGIN INSERT INTO surface_commit_guard VALUES('missing-session'); END;",
            )?;
            Ok(())
        })?;
        let snapshot_key = format!("test_surface:{session}");
        let rebind = ContextSurfaceRebind {
            expected_system_prompt: original.system_prompt.clone(),
            system_prompt: "must roll back".into(),
            metadata: vec![(snapshot_key.clone(), "reject snapshot".into())],
        };
        let mut replacement = previous.clone();
        replacement.tokens += 1;
        assert!(
            store
                .save_context_view_with_surface(&replacement, None, Some(&rebind))
                .is_err()
        );
        assert_eq!(
            serde_json::to_value(store.context_view(&session)?.unwrap())?,
            serde_json::to_value(previous)?
        );
        assert_eq!(
            serde_json::to_value(store.chat_tool_surface(&session)?.unwrap())?,
            serde_json::to_value(original)?
        );
        assert!(store.chat_metadata(&session, &snapshot_key)?.is_none());
        assert!(
            store
                .chat_metadata(&session, &format!("original_tool_surface:{session}"))?
                .is_none()
        );
    }
    Ok(())
}
