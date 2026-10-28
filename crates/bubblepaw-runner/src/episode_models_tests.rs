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

struct NoProvider;
#[async_trait]
impl Provider for NoProvider {
    async fn generate(
        &self,
        _: ModelRequest,
        _: bubblepaw_core::TextCallback,
        _: CancellationToken,
    ) -> Result<bubblepaw_core::ModelResponse> {
        panic!("Unexpected provider call")
    }
}

#[test]
fn episode_prompt_selects_v2_by_default_and_v3_when_configured() {
    use crate::episodes::{EpisodeConfig, EpisodePrompt};
    let registry = Arc::new(ProviderRegistry::new("main", Arc::new(NoProvider)));
    let models = HttpEpisodeModels::from_registry(Default::default(), registry, None).unwrap();
    let input = SummaryInput {
        focus: None,
        session_id: "s".into(),
        previous_summary: None,
        turn_request: None,
        episode_text: String::new(),
        cap: 1234,
    };
    let v2 = fork_requests::summary_instructions(&models, &input);
    let v2_text = EpisodePrompt::V2.text().replace("\r\n", "\n");
    let v3_text = EpisodePrompt::V3.text().replace("\r\n", "\n");
    let v2_head = v2_text.split("{target}").next().unwrap();
    assert!(v2.replace("\r\n", "\n").starts_with(v2_head));
    assert!(!v2.contains("Actions:") && v2.contains("about 1234 tokens"));
    let models = models
        .with_episode_config(EpisodeConfig {
            episode_prompt: EpisodePrompt::V3,
            ..Default::default()
        })
        .unwrap();
    let v3 = fork_requests::summary_instructions(&models, &input);
    assert!(v3.contains("\"Actions:\" section immediately before \"Open:\""));
    assert!(v3.contains("Read, Searched, Edited, Created, Deleted, Moved, Ran"));
    assert!(v3.contains("about 1234 tokens") && !v3.contains("{limit}"));
    // v3 keeps every v2 line except the format line it extends.
    for line in v2_text.lines().filter(|line| !line.starts_with("Format:")) {
        assert!(v3_text.contains(line), "{line}");
    }
}
