//! Chapter metadata compatibility keeps the existing test coverage unchanged.
use super::*;

#[tokio::test]
async fn fallback_decision_uses_successful_luna_calibration_instead_of_first_route() {
    let failed = serve(vec![(StatusCode::FORBIDDEN, json!({}))]).await;
    let luna = serve(vec![luna_decision(0.7)]).await;
    let mut registry = ProviderRegistry::new("main", Arc::new(NoProvider));
    registry.insert_decision("fixture", failed.clone()).unwrap();
    registry.insert_decision("openai", luna.clone()).unwrap();
    let mut roles = ModelRoles::default();
    roles.decider.models = vec!["fixture/jev-1.13-free".into(), "openai/gpt-6-luna".into()];
    let models = HttpEpisodeModels::from_registry(
        EpisodeModelConfig {
            roles,
            retry_base_delay: Duration::ZERO,
        },
        Arc::new(registry),
        None,
    )
    .unwrap();
    let result = models.decide(split()).await.unwrap();
    assert_eq!(result.model, "gpt-6-luna");
    // 0.7 qualifies for Luna's 0.6 threshold, but not the first route's 0.8.
    assert_eq!(result.choice.as_deref(), Some("u2"));
    assert_eq!(failed.decisions.lock().unwrap().len(), 1);
    assert_eq!(luna.decisions.lock().unwrap().len(), 1);
}
