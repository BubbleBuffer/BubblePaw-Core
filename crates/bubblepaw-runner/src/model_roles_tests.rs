use super::*;
use crate::provider::ScriptedProvider;

#[test]
fn decision_roles_have_no_built_in_models() {
    let roles = ModelRoles::for_models("summary/fixture".into(), "main/fixture".into());
    assert!(roles.decider.models.is_empty());
    assert!(roles.approver.models.is_empty());
    assert_eq!(roles.summary.models, ["summary/fixture", "main/fixture"]);
    assert_eq!(roles.chapter.models, roles.summary.models);
    let defaults = ModelRoles::default();
    assert!(defaults.decider.models.is_empty());
    assert!(defaults.approver.models.is_empty());
    defaults.validate().unwrap();
}

#[test]
fn configured_chain_falls_back_in_order_with_its_own_calibration() {
    let roles = ModelRoles::for_models("summary/fixture".into(), "main/fixture".into());
    let mut decider = roles.decider.clone();
    decider.models = vec!["openai/gpt-6-luna".into(), "llm/main/fixture".into()];
    let registry = ProviderRegistry::new("main", Arc::new(ScriptedProvider::new("unused")));
    let selected = decider.available_model(&registry).unwrap();
    assert_eq!(selected, "llm/main/fixture");
    assert_eq!(decider.decision_settings(selected), (3, 0.8));
    assert_eq!(
        (roles.approver.repeats, roles.approver.threshold),
        (None, None)
    );
}

#[tokio::test]
async fn unconfigured_approver_fails_at_use_naming_its_config_key() {
    let registry = Arc::new(ProviderRegistry::new(
        "main",
        Arc::new(ScriptedProvider::new("unused")),
    ));
    let provider = RoleDecisionProvider {
        registry,
        role: ModelRoles::default().approver,
    };
    let error = provider
        .decide(DecisionRequest::default(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("roles.approver"), "{error}");
}

#[test]
fn split_repeat_and_threshold_overrides_are_independent() {
    let mut role = ModelRoles::default().decider;
    role.repeats = Some(2);
    assert_eq!(role.decision_settings("openai/gpt-6-luna"), (2, 0.6));
    assert_eq!(role.decision_settings("llm/main/fixture"), (2, 0.8));
    role.repeats = None;
    role.threshold = Some(0.95);
    assert_eq!(role.decision_settings("openai/gpt-6-luna"), (1, 0.95));
    assert_eq!(role.decision_settings("llm/main/fixture"), (3, 0.95));
    role.repeats = Some(4);
    assert_eq!(role.decision_settings("openai/gpt-6-luna"), (4, 0.95));
    assert_eq!(role.decision_settings("llm/main/fixture"), (4, 0.95));
}
