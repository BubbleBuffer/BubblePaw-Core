use super::*;
use crate::episode_models::EpisodeModels;
use async_trait::async_trait;
use bubblepaw_core::{CancellationToken, ModelRequest, ModelResponse, Provider, TextCallback};
use std::sync::Mutex;

const FIXTURES: &str = include_str!("../../../tests/fixtures/chapter-prompt-bench.json");

struct Recording {
    requests: Mutex<Vec<ModelRequest>>,
    scripted: ScriptedProvider,
}

#[async_trait]
impl Provider for Recording {
    async fn generate(
        &self,
        request: ModelRequest,
        on_text: TextCallback,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse> {
        self.requests.lock().unwrap().push(request.clone());
        self.scripted.generate(request, on_text, cancellation).await
    }
}

fn fixtures() -> Vec<Fixture> {
    serde_json::from_str::<Fixtures>(FIXTURES).unwrap().fixtures
}

#[tokio::test]
async fn production_arms_send_identical_fixture_evidence_and_parse_both_formats() {
    for fixture in fixtures() {
        let recording = Arc::new(Recording {
            requests: Mutex::new(Vec::new()),
            scripted: ScriptedProvider::with_steps(
                [false, true]
                    .into_iter()
                    .map(|structured| ScriptedStep::Text {
                        text: scripted_text(&fixture, structured).unwrap(),
                    })
                    .collect(),
                "",
            ),
        });
        let registry =
            Arc::new(ProviderRegistry::new("opencode", recording.clone()).with_strict_routes());
        let input = ChapterInput {
            session_id: fixture.session_id,
            episodes: fixture.episodes.clone(),
            previous_chapters: fixture.previous_chapters,
            typed_user_texts: fixture.typed_user_texts,
            task_instructions: fixture.task_instructions,
        };
        let v1 = models(registry.clone(), ChapterPrompt::V1)
            .unwrap()
            .close_chapter(input.clone())
            .await
            .unwrap();
        let v2 = models(registry, ChapterPrompt::V2)
            .unwrap()
            .close_chapter(input)
            .await
            .unwrap();
        assert_eq!(v1.description, "Scripted chapter");
        assert!(v1.decisions.is_none() && v1.sections.is_none());
        assert_eq!(v2.decisions, Some(Vec::new()));
        assert_eq!(v2.sections.as_ref().unwrap().len(), 1);
        let requests = recording.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_ne!(requests[0].system_prompt, requests[1].system_prompt);
        assert_eq!(requests[0].messages.len(), 1);
        assert_eq!(requests[1].messages.len(), 1);
        assert_eq!(
            requests[0].messages[0].content,
            requests[1].messages[0].content
        );
        for request in requests.iter() {
            assert!(request.tools.is_empty());
            for episode in &fixture.episodes {
                assert!(request.messages[0].content.contains(episode));
            }
        }
    }
}

#[tokio::test]
async fn dry_run_writes_two_arms_with_shared_input_fingerprints() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("fixtures.json");
    std::fs::write(&source, FIXTURES).unwrap();
    let out = temp.path().join("out");
    let summary = run(
        ChapterBenchOptions {
            fixtures: source,
            out: out.clone(),
            dry_run: true,
            max_cost_usd: 0.01,
        },
        None,
    )
    .await
    .unwrap();
    assert_eq!(summary["fixtures"], 2);
    assert_eq!(summary["acceptance"], "scripted");
    let report: Value =
        serde_json::from_slice(&std::fs::read(out.join("chapter-prompt-bench.json")).unwrap())
            .unwrap();
    assert_eq!(report["spend_usd"], 0.0);
    assert_eq!(report["calls"].as_array().unwrap().len(), 4);
    for fixture in report["fixtures"].as_array().unwrap() {
        for arm in ["v1", "v2"] {
            assert_eq!(
                fixture["arms"][arm]["input_sha256"],
                fixture["input_sha256"]
            );
            assert_eq!(fixture["arms"][arm]["description"], "Scripted chapter");
            assert!(fixture["arms"][arm]["metadata_error"].is_null());
        }
        assert!(fixture["arms"]["v1"]["metadata_text"].is_null());
        let text = fixture["arms"]["v2"]["metadata_text"].as_str().unwrap();
        assert!(text.starts_with("Decisions: none") && text.contains("Sections:"));
    }
}

#[tokio::test]
async fn v2_fallback_reports_raw_blocks_and_parser_error() {
    let fixture = fixtures().remove(0);
    let blocks = "Decisions:\n- Keep the reader\n\nSections:\n- Import repair | episodes 1-9";
    let registry = Arc::new(
        ProviderRegistry::new(
            "opencode",
            Arc::new(ScriptedProvider::with_steps(
                vec![ScriptedStep::Text {
                    text: format!("# Import repair\nProse.\n\n{blocks}\nRelated: none"),
                }],
                "",
            )),
        )
        .with_strict_routes(),
    );
    let input = ChapterInput {
        session_id: fixture.session_id,
        episodes: fixture.episodes,
        previous_chapters: fixture.previous_chapters,
        typed_user_texts: fixture.typed_user_texts,
        task_instructions: fixture.task_instructions,
    };
    let (result, text, error) = models(registry, ChapterPrompt::V2)
        .unwrap()
        .close_chapter_with_metadata(input)
        .await
        .unwrap();
    assert!(result.decisions.is_none() && result.sections.is_none());
    assert!(!result.summary.contains("Decisions:"));
    assert_eq!(text.as_deref(), Some(blocks));
    assert_eq!(error, Some("section_coverage"));
}
