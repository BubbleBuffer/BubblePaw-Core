//! Unit tests for the recorded-history replay driver.
use super::*;

mod archive_gaps;
mod haiku;
mod http_probes;
mod imports;
mod plan_guard;
mod probe_registry;
mod progress_file;
mod readiness;
mod relay;
mod transition;

/// The embedded table plus one test-only decider, so fixtures price without private model ids.
fn example_prices() -> &'static PriceTable {
    static TABLE: std::sync::OnceLock<PriceTable> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table: Value = serde_json::from_str(include_str!("../prices.json")).unwrap();
        table["models"]["example-decider"] = json!({"billing":"list_price","periods":{"off_peak":{"uncached_input":0.1,"cache_read":0.1,"cache_write":0.1,"output":0.0}}});
        PriceTable::from_json(&table.to_string()).unwrap()
    })
}

#[test]
fn scripted_episode_memory_is_valid_prose_not_the_rendered_transcript() {
    let rendered = "[user-1 User]\n[source user-1: Typed request]\nAdd top_words.\n\n[asst-1 Assistant]\nPatching.\nTool call call-1 (js) arguments (9 tokens):\n{\"code\":\"*** Begin Patch\"}\n\n[call-1 Tool]\n[tool output call-1, 30 chars, ok]\n<｜｜DSML｜｜ calls>\nTool call ID: call-1";
    let memory = scripted_episode_memory(rendered, 800);
    assert_eq!(
        crate::episode_models::summary_output_reason("summary", &memory),
        None,
        "{memory}"
    );
    assert!(memory.contains("covered 3 messages"));
    assert!(memory.contains("The user wrote (source user-1): Add top_words."));
    assert!(memory.contains("The agent wrote (source asst-1): Patching."));
    assert!(memory.contains("A tool returned (source call-1): (no plain text)"));
    assert!(!memory.contains("Tool call "));
    let clipped = scripted_episode_memory(rendered, 12);
    assert_eq!(
        crate::episode_models::summary_output_reason("summary", &clipped),
        None
    );
}

fn gap_corpus(messages: Vec<Message>) -> ReplayCorpus {
    ReplayCorpus {
        chat_id: "archive-gap-fixture".into(),
        recorded_model: None,
        context_window: None,
        instructions: String::new(),
        messages,
        message_metadata: json!([]),
        boundary_gold: None,
        relates_gold: vec![],
        provenance: json!({"fixture":true}),
    }
}
fn performance_message(index: usize, role: Role) -> Message {
    Message {
        id: format!("gate-{index}"),
        session_id: "fixture".into(),
        origin: (role == Role::User).then_some(MessageOrigin::Typed),
        role,
        content: format!("Fixture {index}"),
        tool_calls: vec![],
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: "2026-10-10T00:00:00Z".into(),
    }
}

fn config_for_decider(model: &str) -> EpisodeModelConfig {
    let mut config = EpisodeModelConfig {
        retry_base_delay: Duration::ZERO,
        ..Default::default()
    };
    config.roles.decider.models = vec![model.into()];
    config
}
