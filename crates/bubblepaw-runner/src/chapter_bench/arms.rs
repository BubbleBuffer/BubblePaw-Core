//! Independent chapter-selection arms over one frozen episode sequence.
use super::{Arm, episodes::FrozenEpisodes};
use crate::{
    episode_models::{EpisodeModels, SplitInput, SplitOption},
    episode_worker::open_representation,
    replay::ReplayCorpus,
};
use anyhow::{Context, Result, ensure};
use bubblepaw_core::{ContextEpisode, ContextSubjectChapter, SubjectChapterStatus, is_typed_user};
use serde::Serialize;
use std::{collections::HashSet, sync::Arc};

#[derive(Default, Serialize)]
pub(crate) struct Selection {
    pub predicted_ids: Vec<String>,
    pub probes: Vec<Probe>,
}

#[derive(Serialize)]
pub(crate) struct Probe {
    pub first_message_id: String,
    pub mapped_typed_id: Option<String>,
    pub selected: bool,
}

pub(crate) async fn predictions(
    corpus: &ReplayCorpus,
    frozen: &FrozenEpisodes,
    arm: &Arm,
    models: Arc<dyn EpisodeModels>,
    before: &(dyn Fn(&str) + Send + Sync),
    decision_context_tokens: usize,
) -> Result<Selection> {
    ensure!(
        frozen.complete,
        "Chapter arms require completed frozen episodes"
    );
    ensure!(
        frozen.chat == corpus.chat_id,
        "Frozen episodes belong to a different chat"
    );
    ensure!(arm.kind == "decider", "Unknown chapter benchmark arm kind");
    let mut selection = Selection::default();
    let mut predicted = HashSet::new();
    let Some(first) = frozen.episodes.first() else {
        return Ok(selection);
    };
    let mut chapter = chapter(&arm.name, 1, first);
    let mut members = vec![member(first, &chapter.id)];
    for candidate in frozen.episodes.iter().skip(1) {
        let position = corpus
            .messages
            .iter()
            .position(|message| message.id == candidate.first_message_id)
            .context("Frozen candidate boundary is absent from normalized history")?;
        before(&candidate.first_message_id);
        let selected = {
            let result = models
                .decide(SplitInput {
                    session_id: frozen.session_id.clone(),
                    open_chapter: open_representation(
                        Some(&chapter),
                        &members,
                        decision_context_tokens,
                    ),
                    options: vec![SplitOption {
                        message_id: candidate.first_message_id.clone(),
                        user: format!(
                            "EPISODE SUMMARY (not typed user input):\n{}",
                            candidate.summary
                        ),
                        agent_before: String::new(),
                    }],
                })
                .await?;
            ensure!(
                result
                    .choice
                    .as_ref()
                    .is_none_or(|id| id == &candidate.first_message_id),
                "Decider selected a boundary that was not offered"
            );
            result.choice.is_some()
        };
        let mapped_typed_id = corpus.messages[position..]
            .iter()
            .find(|message| is_typed_user(message))
            .map(|message| message.id.clone());
        if selected {
            if let Some(typed) = &mapped_typed_id
                && predicted.insert(typed.clone())
            {
                selection.predicted_ids.push(typed.clone());
            }
            chapter = chapter_for_next(&arm.name, &chapter, candidate);
            members.clear();
        }
        selection.probes.push(Probe {
            first_message_id: candidate.first_message_id.clone(),
            mapped_typed_id,
            selected,
        });
        chapter.last_episode_ordinal = candidate.ordinal;
        members.push(member(candidate, &chapter.id));
    }
    Ok(selection)
}

fn member(episode: &ContextEpisode, chapter_id: &str) -> ContextEpisode {
    let mut copy = episode.clone();
    copy.chapter_id = chapter_id.into();
    copy
}

fn chapter_for_next(
    arm: &str,
    previous: &ContextSubjectChapter,
    episode: &ContextEpisode,
) -> ContextSubjectChapter {
    chapter(arm, previous.ordinal + 1, episode)
}

fn chapter(arm: &str, ordinal: usize, episode: &ContextEpisode) -> ContextSubjectChapter {
    ContextSubjectChapter {
        id: format!("chapter-bench:{arm}:{ordinal}"),
        session_id: episode.session_id.clone(),
        ordinal,
        first_episode_ordinal: episode.ordinal,
        last_episode_ordinal: episode.ordinal,
        status: SubjectChapterStatus::Open,
        description: None,
        summary: None,
        related_ids: vec![],
        related_evidence: vec![],
        model: None,
        usage: None,
        reasoning: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::episode_models::{
        ChapterInput, ChapterResult, SplitResult, SummaryInput, SummaryResult,
    };
    use anyhow::bail;
    use async_trait::async_trait;
    use bubblepaw_core::{Message, Role, Usage};
    use serde_json::{Value, json};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Models(Mutex<Vec<SplitInput>>);
    #[async_trait]
    impl EpisodeModels for Models {
        async fn decide(&self, input: SplitInput) -> Result<SplitResult> {
            let choice = input
                .options
                .first()
                .map(|option| option.message_id.clone());
            self.0.lock().unwrap().push(input);
            Ok(SplitResult {
                choice,
                probabilities: vec![1.0, 0.0],
                model: "scripted".into(),
                usage: Usage::default(),
                reasoning: None,
            })
        }
        async fn summarize(&self, _: SummaryInput) -> Result<SummaryResult> {
            bail!("No summaries in selection arms")
        }
        async fn close_chapter(&self, _: ChapterInput) -> Result<ChapterResult> {
            bail!("No chapter summaries in selection arms")
        }
    }

    fn fixture() -> Result<(ReplayCorpus, FrozenEpisodes)> {
        let messages: Vec<_> = (0..6)
            .map(|index| Message {
                id: format!("r{index}"),
                session_id: "source".into(),
                role: if index % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                origin: None,
                content: format!("Synthetic message {index}"),
                tool_calls: vec![],
                tool_call_id: None,
                provider_state: None,
                is_error: false,
                created_at: String::new(),
            })
            .collect();
        let episodes = (0..3).map(|index| serde_json::from_value(json!({
            "id":format!("e{index}"),"session_id":"generated","ordinal":index+1,
            "first_message_id":format!("r{}",index*2),"last_message_id":format!("r{}",index*2+1),
            "source_tokens":10,"cap":100,"origin_request_id":null,"title":"Synthetic",
            "summary":format!("Unique summary {index}"),"model":"scripted","usage":null,
            "created_at":"","chapter_id":"generation-only"
        }))).collect::<std::result::Result<Vec<ContextEpisode>, _>>()?;
        let corpus = ReplayCorpus {
            chat_id: "synthetic".into(),
            recorded_model: None,
            context_window: None,
            instructions: String::new(),
            messages,
            message_metadata: Value::Null,
            boundary_gold: None,
            relates_gold: vec![],
            provenance: Value::Null,
        };
        let frozen = FrozenEpisodes {
            chat: corpus.chat_id.clone(),
            mode: "scripted".into(),
            source_fingerprint: String::new(),
            config_fingerprint: String::new(),
            session_id: "generated".into(),
            stage_id: "stage-test".into(),
            complete: true,
            status: "complete".into(),
            messages_total: 6,
            messages_imported: 6,
            covered_messages: 6,
            coverage_through_message_id: Some("r5".into()),
            uncovered_tail_messages: 0,
            episodes,
        };
        Ok((corpus, frozen))
    }

    #[tokio::test]
    async fn decider_uses_only_its_accumulated_chapter_and_resets_after_selection() -> Result<()> {
        let (corpus, frozen) = fixture()?;
        let models = Arc::new(Models::default());
        let arm = Arm {
            name: "jev".into(),
            kind: "decider".into(),
            model: "scripted/jev".into(),
        };
        let output = predictions(&corpus, &frozen, &arm, models.clone(), &|_| {}, 1000).await?;
        assert_eq!(output.predicted_ids, vec!["r2", "r4"]);
        let inputs = models.0.lock().unwrap();
        assert!(inputs[0].open_chapter.contains("Unique summary 0"));
        assert!(inputs[1].open_chapter.contains("Unique summary 1"));
        assert!(!inputs[1].open_chapter.contains("Unique summary 0"));
        assert_eq!(
            inputs[0].options[0].user,
            "EPISODE SUMMARY (not typed user input):\nUnique summary 1"
        );
        assert!(inputs[0].options[0].agent_before.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn non_user_candidates_map_to_the_next_typed_request_and_deduplicate() -> Result<()> {
        let (mut corpus, frozen) = fixture()?;
        corpus.messages[2].role = Role::Assistant;
        let arm = Arm {
            name: "jev".into(),
            kind: "decider".into(),
            model: "scripted/jev".into(),
        };
        let output = predictions(
            &corpus,
            &frozen,
            &arm,
            Arc::new(Models::default()),
            &|_| {},
            1000,
        )
        .await?;
        assert_eq!(output.predicted_ids, vec!["r4"]);
        assert_eq!(output.probes.len(), 2);
        assert!(
            output
                .probes
                .iter()
                .all(|probe| probe.selected && probe.mapped_typed_id.as_deref() == Some("r4"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn selected_boundary_without_a_later_typed_request_remains_in_probe_audit() -> Result<()>
    {
        let (mut corpus, frozen) = fixture()?;
        corpus.messages[4].role = Role::Assistant;
        let arm = Arm {
            name: "jev".into(),
            kind: "decider".into(),
            model: "scripted/jev".into(),
        };
        let output = predictions(
            &corpus,
            &frozen,
            &arm,
            Arc::new(Models::default()),
            &|_| {},
            1000,
        )
        .await?;
        assert_eq!(output.predicted_ids, vec!["r2"]);
        assert_eq!(output.probes[1].first_message_id, "r4");
        assert!(output.probes[1].selected);
        assert!(output.probes[1].mapped_typed_id.is_none());
        Ok(())
    }
}
