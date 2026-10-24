use super::*;

#[test]
fn episode_error_usage_preserves_classification_publication_and_all_billing_fields() {
    let usage = Usage {
        input_tokens: 111,
        output_tokens: 17,
        cache_read_tokens: 100,
        cache_creation_tokens: 7,
        reasoning_tokens: Some(12),
    };
    let original = bubblepaw_core::ProviderError::new(
        bubblepaw_core::ProviderErrorKind::Transient,
        "classified transport failure",
    )
    .with_published(true);
    let error = with_episode_usage(
        anyhow::Error::new(original).context("outer diagnostic"),
        usage.clone(),
    );
    let classified = bubblepaw_core::ProviderError::of(&error).unwrap();
    assert_eq!(
        classified.kind,
        bubblepaw_core::ProviderErrorKind::Transient
    );
    assert!(classified.published);
    assert_eq!(classified.usage, Some(usage));
    let decision = decision_error_usage(&error).unwrap();
    assert_eq!((decision.input_tokens, decision.output_tokens), (111, 17));
}
