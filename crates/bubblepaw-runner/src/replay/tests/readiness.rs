//! Replay readiness against the prefix planner.
use super::*;

#[test]
fn replay_readiness_matches_prefix_planner_with_multi_turn_episodes_and_parallel_tools()
-> Result<()> {
    let mut messages = vec![
        performance_message(0, Role::User),
        performance_message(1, Role::Assistant),
        performance_message(2, Role::User),
        performance_message(3, Role::Assistant),
        performance_message(4, Role::Tool),
        performance_message(5, Role::Tool),
        performance_message(6, Role::Assistant),
        performance_message(7, Role::User),
        performance_message(8, Role::Assistant),
        performance_message(9, Role::Assistant),
        performance_message(10, Role::User),
        performance_message(11, Role::Assistant),
    ];
    messages[2].content = "[new subject] Change task".into();
    messages[3].tool_calls = vec![
        bubblepaw_core::ToolCall {
            id: "parallel-a".into(),
            name: "recorded_only".into(),
            arguments: json!({}),
        },
        bubblepaw_core::ToolCall {
            id: "parallel-b".into(),
            name: "recorded_only".into(),
            arguments: json!({}),
        },
    ];
    messages[4].tool_call_id = Some("parallel-a".into());
    messages[5].tool_call_id = Some("parallel-b".into());
    // Exact synthetic canonical counts exercise threshold edges without large text.
    let counts = [
        12, 10_000, 12, 4_000, 4_000, 8_000, 12, 12, 70_000, 33_000, 12, 12,
    ];
    let mut gate = ReplayWorkerGate::new();
    let mut pending = HashSet::new();
    let mut coverage = 0;
    let mut drained_ranges = Vec::new();
    let mut ready_steps = Vec::new();
    for (index, (message, tokens)) in messages.iter().zip(counts).enumerate() {
        gate.observe(message, tokens)?;
        for call in &message.tool_calls {
            pending.insert(call.id.clone());
        }
        if let Some(call) = &message.tool_call_id {
            pending.remove(call);
        }
        let history = &messages[..=index];
        let oracle = crate::episodes::plan_episodes(
            history,
            coverage,
            |message| gate.canonical_tokens[&message.id],
            is_typed_user,
        )?;
        let ready = gate.needs_lcm(history, &pending)?;
        assert_eq!(
            ready,
            !oracle.episodes.is_empty(),
            "step {} coverage {}",
            index + 1,
            coverage
        );
        if ready {
            ready_steps.push(index + 1);
        }
        // Simulate the worker's repeated first-episode drain. Chapter decisions
        // can select only the completed episode's start, leaving coverage intact.
        loop {
            let plan = crate::episodes::plan_episodes(
                history,
                coverage,
                |message| gate.canonical_tokens[&message.id],
                is_typed_user,
            )?;
            let Some(first) = plan.episodes.into_iter().next() else {
                break;
            };
            drained_ranges.push((first.message_range.start, first.message_range.end));
            coverage = first.message_range.end;
        }
        gate.coverage_end = coverage;
    }
    assert_eq!(
        drained_ranges[0],
        (0, 6),
        "Both typed turns and parallel results stay in one episode"
    );
    assert!(
        !ready_steps.contains(&4),
        "Pending parallel calls cannot make a size episode ready"
    );
    assert!(
        !ready_steps.contains(&5),
        "One of two parallel results is still pending"
    );
    assert!(
        ready_steps.contains(&6),
        "Completing the oversized parallel round creates an episode"
    );
    Ok(())
}

#[test]
fn replay_readiness_keeps_pending_call_errors_at_first_activation_and_new_typed_input() -> Result<()>
{
    let mut messages = vec![
        performance_message(0, Role::User),
        performance_message(1, Role::Assistant),
        performance_message(2, Role::User),
    ];
    messages[1].tool_calls.push(bubblepaw_core::ToolCall {
        id: "unfinished".into(),
        name: "recorded_only".into(),
        arguments: json!({}),
    });
    for activate_at_end in [false, true] {
        let mut gate = ReplayWorkerGate::new();
        let mut pending = HashSet::new();
        for (index, message) in messages.iter().enumerate() {
            gate.observe(message, 1)?;
            for call in &message.tool_calls {
                pending.insert(call.id.clone());
            }
            if index < 2 && !activate_at_end {
                assert!(!gate.needs_lcm(&messages[..=index], &pending)?);
            }
        }
        let error = gate
            .needs_lcm(&messages, &pending)
            .expect_err("Typed input interrupting a tool round must still fail");
        assert!(error.to_string().contains("incomplete tool round"));
    }
    Ok(())
}

#[test]
fn replay_readiness_uses_the_workers_configured_source_policy() -> Result<()> {
    let config = EpisodeConfig {
        source_target_tokens: 2_000,
        source_max_tokens: 4_000,
        ..EpisodeConfig::default()
    };
    let messages = [
        performance_message(0, Role::User),
        performance_message(1, Role::Assistant),
        performance_message(2, Role::User),
        performance_message(3, Role::Assistant),
    ];
    let mut gate = ReplayWorkerGate::with_config(config.clone());
    let mut readiness = Vec::new();
    for (index, (message, tokens)) in messages.iter().zip([1, 1_500, 1, 1_500]).enumerate() {
        gate.observe(message, tokens)?;
        let history = &messages[..=index];
        let plan = crate::episodes::plan_episodes_with_config(
            history,
            0,
            |message| gate.canonical_tokens[&message.id],
            is_typed_user,
            &config,
        )?;
        let ready = gate.needs_lcm(history, &HashSet::new())?;
        assert_eq!(ready, !plan.episodes.is_empty());
        readiness.push(ready);
    }
    assert_eq!(readiness, [false, false, false, true]);
    Ok(())
}
