use super::*;

#[tokio::test]
async fn existing_children_refresh_root_tools_each_turn_and_keep_profile_restrictions() -> Result<()>
{
    let fixture = Fixture::new(None)?;
    let read = fixture
        .manager
        .start(&fixture.owner, json!({"task":"first"}))?;
    let write = fixture.manager.start(
        &fixture.owner,
        json!({"task":"first","profile":"workspace"}),
    )?;
    fixture.settle(&fixture.owner.session_id, &read).await?;
    fixture.settle(&fixture.owner.session_id, &write).await?;
    assert!(
        fixture
            .description(read["sessionId"].as_str().unwrap())
            .contains("old_read")
    );
    {
        let mut tools = fixture.tools.write().unwrap();
        let retained = tools
            .capabilities()
            .into_iter()
            .filter(|tool| tool.name != "old_read")
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        *tools = tools.restricted_capabilities(&retained);
        for (name, read_only) in [("new_read", true), ("new_write", false)] {
            tools.register_host(Arc::new(Capability {
                name,
                read_only,
                calls: fixture.effects.clone(),
            }))?;
        }
    }
    for (receipt, writable) in [(&read, false), (&write, true)] {
        fixture.manager.send(
            &fixture.owner,
            json!({"sessionId":receipt["sessionId"],"text":"next"}),
        )?;
        fixture.settle(&fixture.owner.session_id, receipt).await?;
        let session = receipt["sessionId"].as_str().unwrap();
        let messages = fixture.store.messages(session)?;
        let result = messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Tool)
            .expect("the child executed live tools.$list discovery");
        assert!(!result.is_error, "{}", result.content);
        let listed: Value =
            serde_json::from_str(result.content.split("\n[---]\n").next().unwrap())?;
        let names = listed
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(names.contains(&"new_read"));
        assert!(!names.contains(&"old_read"));
        assert!(names.contains(&"agent_send"));
        assert_eq!(
            names.contains(&"new_write"),
            writable,
            "workspace profile alone may discover writing tools"
        );
        let requests = fixture.probe.requests.lock().unwrap();
        let requests = requests
            .iter()
            .filter(|request| request.session_id == session)
            .collect::<Vec<_>>();
        assert_eq!(
            requests.len(),
            3,
            "initial reply, discovery call, final reply"
        );
        let frozen = serde_json::to_value(&requests[0].tools)?;
        for request in &requests {
            assert_eq!(serde_json::to_value(&request.tools)?, frozen);
            assert_eq!(request.system_prompt, requests[0].system_prompt);
        }
        let description = &requests.last().unwrap().tools[0].description;
        assert!(description.contains("old_read"));
        assert!(
            !description.contains("new_read") && !description.contains("new_write"),
            "the initial JS declaration stays frozen while discovery follows the live profile"
        );
    }
    fixture.manager.shutdown().await?;
    Ok(())
}
