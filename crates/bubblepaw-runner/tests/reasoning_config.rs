//! Provider-aware replay defaults, without constructing providers or reading keys.
use bubblepaw_core::ReasoningReplay;
use bubblepaw_runner::config::ProviderSettings;

fn settings(kind: &str, replay: &str) -> ProviderSettings {
    toml::from_str(&format!(
        "kind = '{kind}'\nbase_url = 'http://127.0.0.1:1/v1'\nkey_env = 'SYNTHETIC_UNUSED_KEY'\n[[models]]\nid = 'model.one'\n{replay}"
    ))
    .unwrap()
}

#[test]
fn omitted_chat_policy_uses_the_adapter_default_and_explicit_policies_survive() {
    let config = settings("chat-completions", "");
    config.validate("local").unwrap();
    assert!(config.reasoning_replay().is_empty());
    assert_eq!(ReasoningReplay::default(), ReasoningReplay::ToolLoop);
    for policy in [
        ReasoningReplay::None,
        ReasoningReplay::ToolLoop,
        ReasoningReplay::All,
    ] {
        let config = settings(
            "chat-completions",
            &format!(
                "reasoning_replay = {}",
                serde_json::to_string(&policy).unwrap()
            ),
        );
        config.validate("local").unwrap();
        assert_eq!(config.reasoning_replay()["model.one"], policy);
    }
}

#[test]
fn non_chat_models_accept_omission_without_enabling_chat_policy() {
    for kind in ["responses", "anthropic-messages"] {
        for setting in ["", "reasoning_replay = 'none'"] {
            settings(kind, setting).validate("local").unwrap();
        }
        for setting in ["reasoning_replay = 'tool_loop'", "reasoning_replay = 'all'"] {
            assert!(settings(kind, setting).validate("local").is_err());
        }
    }
}
