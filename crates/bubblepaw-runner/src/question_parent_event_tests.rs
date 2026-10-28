//! Parent events describe the same durable question transition as the child row.
use super::*;
use bubblepaw_core::{EventKind, HarnessEvent};

fn question_events(harness: &Harness, parent: &str, cursor: u64) -> Result<Vec<HarnessEvent>> {
    let _ = harness.last_sequence();
    Ok(harness.events_after(cursor, Some(parent), 100)?.into_iter().filter(|event| {
        matches!(&event.kind, EventKind::HostStateChanged { key, .. } if key == "child_question" || key == "child_question_answer")
    }).collect())
}

async fn transition_case(store: &Store) -> Result<()> {
    let parent = store.create_session("parent")?.id;
    let questions = Arc::new(QuestionTools::new(store.clone(), Arc::default(), true));
    let harness = Arc::new(test_harness(store));
    questions.set_child_answer_harness(harness.clone())?;
    for mode in ["parent", "user", "cancelled"] {
        let child = store
            .create_session_with_parent(mode, Default::default(), Some(&parent))?
            .id;
        let cursor = harness.last_sequence();
        let before_ask = store.storage_stats()?;
        let task = {
            let (questions, child) = (questions.clone(), child.clone());
            tokio::spawn(async move {
                questions
                    .ask(
                        context(&child),
                        json!({"text":"Which colour?","options":["Red","Blue"]}),
                    )
                    .await
            })
        };
        let question = pending(&questions, &child).await?;
        // Synchronize on the journal's snapshot lock as well as the source row:
        // directory storage commits canonical state before serving its events.
        let asked_events = question_events(&harness, &parent, cursor)?;
        let after_ask = store.storage_stats()?;
        assert_eq!(
            after_ask.chat_transaction_count - before_ask.chat_transaction_count,
            1
        );
        assert_eq!(asked_events.len(), 1);
        let EventKind::HostStateChanged { key, value: asked } = &asked_events[0].kind else {
            unreachable!()
        };
        assert_eq!(key, "child_question");
        assert_eq!(asked["status"], "pending");
        assert_eq!(asked["child_session_id"], child);
        assert_eq!(asked["parent_session_id"], parent);
        assert_eq!(asked["id"], question["id"]);
        assert_eq!(asked["text"], "Which colour?");
        assert_eq!(asked["options"], json!(["Red", "Blue"]));
        assert!(asked["asked_at"].as_str().is_some());
        assert_eq!(asked["run_id"], "question-test");
        assert_eq!(asked.as_object().unwrap().len(), 8);
        assert!(asked_events[0].durable);
        let id = question["id"].as_str().unwrap();
        let before_resolve = store.storage_stats()?;
        match mode {
            "parent" => {
                assert!(
                    questions
                        .answer(&harness, &child, id, "Blue with stripes")?
                        .live
                );
            }
            "user" => {
                questions
                    .answer_child_as_user(&parent, &child, id, "Blue with stripes")
                    .await?;
            }
            _ => {
                assert!(questions.cancel(&child)?);
            }
        }
        let result: Value = serde_json::from_str(&task.await??.content)?;
        let resolved_events = question_events(&harness, &parent, asked_events[0].sequence)?;
        let after_resolve = store.storage_stats()?;
        // Answers accept one generated child input after the atomic state
        // transaction; cancellation needs only the state transaction.
        let expected_writes = if mode == "cancelled" { 1 } else { 2 };
        assert_eq!(
            after_resolve.chat_transaction_count - before_resolve.chat_transaction_count,
            expected_writes
        );
        assert_eq!(resolved_events.len(), 1);
        let EventKind::HostStateChanged {
            key,
            value: resolved,
        } = &resolved_events[0].kind
        else {
            unreachable!()
        };
        let expected_status = if mode == "cancelled" {
            "cancelled"
        } else {
            "answered"
        };
        assert_eq!(
            key,
            if mode == "cancelled" {
                "child_question"
            } else {
                "child_question_answer"
            }
        );
        assert_eq!(resolved["status"], expected_status);
        assert_eq!(result["status"], expected_status);
        for field in [
            "child_session_id",
            "id",
            "parent_session_id",
            "text",
            "options",
            "asked_at",
            "run_id",
        ] {
            assert_eq!(resolved[field], asked[field]);
        }
        if mode != "cancelled" {
            assert_eq!(resolved["answer"], "Blue with stripes");
            assert_eq!(resolved["answered_by"], mode);
            assert_eq!(resolved.as_object().unwrap().len(), 10);
        } else {
            assert!(resolved.get("answer").is_none());
            assert!(resolved.get("answered_by").is_none());
        }
        assert!(resolved.get("delivery").is_none());
        assert!(resolved_events[0].durable);
        assert!(store.host_state(&parent, "question")?.is_none());
        assert_eq!(question_events(&harness, &parent, cursor)?.len(), 2);
        if mode == "parent" {
            let before_retry = store.storage_stats()?.chat_transaction_count;
            questions.answer(&harness, &child, id, "Blue with stripes")?;
            assert_eq!(store.storage_stats()?.chat_transaction_count, before_retry);
            assert_eq!(question_events(&harness, &parent, cursor)?.len(), 2);
        }
    }
    harness.shutdown(Duration::from_secs(2)).await?;
    Ok(())
}

#[tokio::test]
async fn parent_stream_has_pending_parent_user_answer_and_cancelled_question_events() -> Result<()>
{
    transition_case(&Store::in_memory()?).await
}

#[tokio::test]
async fn directory_parent_question_events_use_one_state_transaction_per_transition() -> Result<()> {
    let directory = tempfile::tempdir()?;
    transition_case(&Store::open_data_dir(directory.path())?).await
}
