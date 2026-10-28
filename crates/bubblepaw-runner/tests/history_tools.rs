use bubblepaw_core::{
    CancellationToken, ContextChapter, ContextCheckpoint, ContextEpisode, ContextSubjectChapter,
    EpisodeBatch, Harness, HarnessConfig, Message, Role, RunOptions, Store, SubjectChapterStatus,
    ToolCall, ToolContext, ToolRegistry,
};
use bubblepaw_runner::{
    artifacts::ArtifactStore,
    codemode::CodeMode,
    provider::{ScriptedProvider, ScriptedStep},
    tools::register_history_tools,
};
use serde_json::{Value, json};
use std::sync::Arc;

struct Fixture {
    root: tempfile::TempDir,
    store: Store,
    session: String,
    registry: ToolRegistry,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path().join("history.sqlite")).unwrap();
        let session = store.create_session("history").unwrap().id;
        let mut registry = ToolRegistry::new();
        register_history_tools(&mut registry, store.clone()).unwrap();
        Self {
            root,
            store,
            session,
            registry,
        }
    }
    // Synthetic canonical rows allow arbitrary tool-argument and control-character
    // payloads without calling any external effects or model.
    fn insert(
        &self,
        session: &str,
        role: Role,
        text: String,
        calls: Vec<ToolCall>,
        link: Option<&str>,
    ) -> Message {
        let message = Message {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: session.into(),
            role,
            content: text,
            tool_calls: calls,
            tool_call_id: link.map(str::to_owned),
            origin: None,
            provider_state: None,
            is_error: false,
            created_at: "2026-10-06T00:00:00Z".into(),
        };
        self.store
            .append_batch(std::slice::from_ref(&message))
            .unwrap();
        message
    }
    fn message(&self, text: &str) -> Message {
        self.insert(&self.session, Role::User, text.into(), vec![], None)
    }
    fn chapter(&self, ordinal: usize, first: &Message, last: &Message, title: &str) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        self.store
            .save_context_chapter(&ContextChapter {
                id: id.clone(),
                session_id: self.session.clone(),
                ordinal,
                first_message_id: first.id.clone(),
                last_message_id: last.id.clone(),
                title: title.into(),
                summary: "Lossy summary, exact detail omitted.".into(),
                model: "scripted".into(),
                usage: None,
                created_at: first.created_at.clone(),
            })
            .unwrap();
        id
    }
    fn episode_batch(
        &self,
        ordinal: usize,
        range: &[Message],
        chapter: usize,
        first_episode: usize,
    ) -> EpisodeBatch {
        let chapter_id = format!("{}:subject:{chapter}", self.session);
        EpisodeBatch {
            episode: ContextEpisode {
                id: format!("{}:episode:{ordinal}", self.session),
                session_id: self.session.clone(),
                ordinal,
                first_message_id: range.first().unwrap().id.clone(),
                last_message_id: range.last().unwrap().id.clone(),
                source_tokens: 32000,
                cap: 2000,
                origin_request_id: None,
                title: format!("Episode {ordinal}"),
                summary: "Lossy episode summary".into(),
                model: "fake".into(),
                usage: None,
                reasoning: None,
                created_at: range.last().unwrap().created_at.clone(),
                chapter_id: chapter_id.clone(),
            },
            decisions: vec![],
            open_chapter: ContextSubjectChapter {
                id: chapter_id,
                session_id: self.session.clone(),
                ordinal: chapter,
                first_episode_ordinal: first_episode,
                last_episode_ordinal: ordinal,
                status: SubjectChapterStatus::Open,
                description: None,
                summary: None,
                related_ids: vec![],
                related_evidence: vec![],
                decisions: None,
                sections: None,
                model: None,
                usage: None,
                reasoning: None,
            },
            closed_chapter: None,
            close_after_episode: None,
            expected_previous_coverage: self.store.episode_coverage_token(&self.session).unwrap(),
        }
    }
    fn context(&self, session: &str) -> ToolContext {
        ToolContext {
            session_id: session.into(),
            run_id: "test-run".into(),
            invocation_id: uuid::Uuid::new_v4().to_string(),
            cancellation: CancellationToken::new(),
            approval: Default::default(),
        }
    }
    async fn call(&self, name: &str, input: Value) -> Value {
        let result = self
            .registry
            .get_host(name)
            .unwrap()
            .invoke(self.context(&self.session), input)
            .await
            .unwrap();
        assert!(!result.is_error && !result.pending);
        assert!(result.content.len() <= 60 * 1024);
        assert!(
            tiktoken_rs::o200k_base_singleton()
                .encode_ordinary(&result.content)
                .len()
                <= 9_000
        );
        serde_json::from_str(&result.content).unwrap()
    }
}

#[tokio::test]
async fn episode_index_search_and_id_filters_cover_closed_open_and_legacy_ranges() {
    let fixture = Fixture::new();
    let legacy = fixture.message("match legacy");
    let legacy_id = fixture.chapter(1, &legacy, &legacy, "Old subject");
    let history: Vec<_> = (0..6)
        .map(|index| fixture.message(&format!("match original {index}")))
        .collect();
    let mut first = fixture.episode_batch(1, &history[..2], 1, 1);
    let chapter_one = first.open_chapter.id.clone();
    let episode_one = first.episode.id.clone();
    first.close_after_episode = Some(close_subject(
        first.open_chapter.clone(),
        "First subject",
        vec![],
    ));
    fixture.store.save_episode_batch(&first).unwrap();
    let mut second = fixture.episode_batch(2, &history[2..4], 2, 2);
    let chapter_two = second.open_chapter.id.clone();
    let episode_two = second.episode.id.clone();
    second.close_after_episode = Some(close_subject(
        second.open_chapter.clone(),
        "Returned subject",
        vec![chapter_one.clone()],
    ));
    fixture.store.save_episode_batch(&second).unwrap();
    let third = fixture.episode_batch(3, &history[4..], 3, 3);
    let chapter_three = third.open_chapter.id.clone();
    fixture.store.save_episode_batch(&third).unwrap();
    let fourth_message = fixture.message("match next open episode");
    let fourth = fixture.episode_batch(4, std::slice::from_ref(&fourth_message), 3, 3);
    fixture.store.save_episode_batch(&fourth).unwrap();
    fixture.message("match uncovered tail");
    let before = fixture.store.storage_stats().unwrap().transaction_count;
    let index = fixture.call("history_chapters", json!({})).await;
    assert_eq!(index["chapters"].as_array().unwrap().len(), 4);
    assert_eq!(index["chapters"][0]["kind"], "legacy");
    assert_eq!(index["chapters"][1]["id"], chapter_one);
    assert_eq!(index["chapters"][1]["status"], "closed");
    assert_eq!(index["chapters"][1]["description"], "First subject");
    assert_eq!(
        index["chapters"][1]["episodes"],
        json!([{"id":episode_one,"ordinal":1,"title":"Episode 1","from":1,"to":2}])
    );
    assert_eq!(index["chapters"][2]["related_ids"], json!([chapter_one]));
    assert_eq!(index["chapters"][3]["status"], "open");
    assert_eq!(index["chapters"][3]["description"], "open");
    assert_eq!(index["chapters"][3]["first_episode_ordinal"], 3);
    assert_eq!(index["chapters"][3]["last_episode_ordinal"], 4);
    assert_eq!(
        index["chapters"][3]["episodes"].as_array().unwrap().len(),
        2
    );
    assert!(
        index["chapters"]
            .as_array()
            .unwrap()
            .iter()
            .all(|chapter| chapter.get("summary").is_none())
    );
    let search = fixture
        .call("history_search", json!({"query":"match"}))
        .await;
    assert_eq!(search["matches"][0]["chapter_id"], legacy_id);
    assert_eq!(search["matches"][1]["episode_id"], episode_one);
    assert_eq!(search["matches"][1]["episode_title"], "Episode 1");
    assert_eq!(search["matches"][1]["chapter_description"], "First subject");
    assert_eq!(search["matches"][8]["chapter_id"], Value::Null);
    assert_eq!(search["matches"][8]["episode_id"], Value::Null);
    let outside = fixture
        .registry
        .get_host("history_read")
        .unwrap()
        .invoke(
            fixture.context(&fixture.session),
            json!({"episode":episode_two,"from":0}),
        )
        .await
        .unwrap_err();
    assert!(outside.to_string().contains("outside the requested"));
    for (filter, value, expected) in [
        ("chapter", legacy_id, vec![0]),
        ("chapter", chapter_two, vec![3, 4]),
        ("chapter", chapter_three, vec![5, 6, 7]),
        ("episode", episode_two, vec![3, 4]),
    ] {
        let mut args = json!({"query":"match"});
        args[filter] = json!(value);
        let search = fixture.call("history_search", args).await;
        assert_eq!(
            search["matches"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| entry["index"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            expected
        );
        let mut args = json!({"count":1});
        args[filter] = json!(value);
        let read = fixture.call("history_read", args.clone()).await;
        assert_eq!(read["messages"][0]["index"], expected[0]);
        if expected.len() > 1 {
            args["from"] = read["next"].clone();
            assert_eq!(
                fixture.call("history_read", args).await["messages"][0]["index"],
                expected[1]
            );
        } else {
            assert_eq!(read["next"], Value::Null);
        }
    }
    assert_eq!(
        fixture.store.storage_stats().unwrap().transaction_count,
        before
    );
}

fn close_subject(
    mut chapter: ContextSubjectChapter,
    description: &str,
    related: Vec<String>,
) -> ContextSubjectChapter {
    chapter.status = SubjectChapterStatus::Closed;
    chapter.description = Some(description.into());
    chapter.summary = Some("Lossy chapter summary".into());
    chapter.model = Some("fake".into());
    chapter.related_evidence = related
        .iter()
        .map(|_| "return to earlier goal".into())
        .collect();
    chapter.related_ids = related;
    chapter
}

#[tokio::test]
async fn id_lookup_returns_original_outputs_and_call_arguments_with_existing_caps() {
    let fixture = Fixture::new();
    let arguments = json!({"code":format!("{} original-final-argument", "x".repeat(2500))});
    let call_message = fixture.insert(
        &fixture.session,
        Role::Assistant,
        "original assistant".into(),
        vec![ToolCall {
            id: "original-call".into(),
            name: "js".into(),
            arguments: arguments.clone(),
        }],
        None,
    );
    let output = fixture.insert(
        &fixture.session,
        Role::Tool,
        format!("original output {}", "🦊".repeat(4500)),
        vec![],
        Some("original-call"),
    );
    let by_message = fixture
        .call("history_read", json!({"message_id":call_message.id}))
        .await;
    let by_call = fixture
        .call("history_read", json!({"tool_call_id":"original-call"}))
        .await;
    assert_eq!(by_message, by_call);
    assert_eq!(
        by_call["messages"][0]["tool_calls"][0]["arguments"],
        arguments
    );
    assert_eq!(by_call["messages"][0]["message_id"], call_message.id);
    assert_eq!(by_call["next"], Value::Null);
    let result = fixture
        .call("history_read", json!({"message_id":output.id}))
        .await;
    let preview = result["messages"][0]["text"].as_str().unwrap();
    assert!(preview.starts_with("original output "));
    assert!(output.content.starts_with(preview));
    assert!(preview.chars().count() <= 4000);
    assert_eq!(result["messages"][0]["truncated"], true);
    assert_eq!(result["messages"][0]["tool_call_id"], "original-call");
    assert_eq!(result["next"], Value::Null);
    let found = fixture
        .call("history_search", json!({"query":"original-final-argument"}))
        .await;
    assert_eq!(found["matches"][0]["message_id"], call_message.id);
    assert_eq!(
        fixture
            .call(
                "history_read",
                json!({"message_id":found["matches"][0]["message_id"]})
            )
            .await,
        by_message
    );
    let schema = fixture
        .registry
        .get_host("history_read")
        .unwrap()
        .definition();
    assert_eq!(
        schema.input_schema["properties"]["message_id"]["type"],
        "string"
    );
    assert_eq!(
        schema.input_schema["properties"]["tool_call_id"]["type"],
        "string"
    );
}

#[tokio::test]
async fn id_filters_reject_conflicts_foreign_ids_and_forks_keep_only_their_originals() {
    let fixture = Fixture::new();
    let first = fixture.message("original local");
    let batch = fixture.episode_batch(1, std::slice::from_ref(&first), 1, 1);
    fixture.store.save_episode_batch(&batch).unwrap();
    let foreign = fixture.store.create_session("foreign").unwrap();
    let foreign_message = fixture.insert(
        &foreign.id,
        Role::Assistant,
        "private foreign".into(),
        vec![ToolCall {
            id: "foreign-call".into(),
            name: "js".into(),
            arguments: json!({"private":true}),
        }],
        None,
    );
    let mut foreign_batch = fixture.episode_batch(1, std::slice::from_ref(&foreign_message), 1, 1);
    foreign_batch.expected_previous_coverage = None;
    foreign_batch.episode.session_id = foreign.id.clone();
    foreign_batch.episode.id = "foreign-episode".into();
    foreign_batch.episode.chapter_id = "foreign-chapter".into();
    foreign_batch.open_chapter.session_id = foreign.id.clone();
    foreign_batch.open_chapter.id = "foreign-chapter".into();
    fixture.store.save_episode_batch(&foreign_batch).unwrap();
    for (name, args, error_text) in [
        (
            "history_search",
            json!({"query":"original","chapter":batch.open_chapter.id,"episode":batch.episode.id}),
            "mutually exclusive",
        ),
        (
            "history_read",
            json!({"chapter":batch.open_chapter.id,"episode":batch.episode.id}),
            "mutually exclusive",
        ),
        (
            "history_read",
            json!({"message_id":first.id,"tool_call_id":"foreign-call"}),
            "mutually exclusive",
        ),
        (
            "history_read",
            json!({"message_id":first.id,"count":1}),
            "cannot be combined",
        ),
        (
            "history_read",
            json!({"tool_call_id":"foreign-call","from":0}),
            "cannot be combined",
        ),
        (
            "history_read",
            json!({"message_id":foreign_message.id}),
            "calling session",
        ),
        (
            "history_read",
            json!({"tool_call_id":"foreign-call"}),
            "calling session",
        ),
        (
            "history_read",
            json!({"chapter":batch.open_chapter.id,"from":1}),
            "existing 0-based",
        ),
        (
            "history_read",
            json!({"episode":"missing"}),
            "calling session",
        ),
        (
            "history_read",
            json!({"episode":"foreign-episode"}),
            "calling session",
        ),
        (
            "history_search",
            json!({"query":"private","chapter":"foreign-chapter"}),
            "calling session",
        ),
        ("history_read", json!({"episode":1}), "string"),
        ("history_read", json!({"message_id":""}), "nonempty"),
        ("history_read", json!({"chapter":""}), "nonempty"),
    ] {
        let error = fixture
            .registry
            .get_host(name)
            .unwrap()
            .invoke(fixture.context(&fixture.session), args.clone())
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains(error_text),
            "{name} {args}: {error}"
        );
    }
    let fork = fixture.store.fork(&fixture.session, "fork").unwrap();
    fixture.insert(
        &fixture.session,
        Role::Assistant,
        "ambiguous one".into(),
        vec![ToolCall {
            id: "duplicate".into(),
            name: "js".into(),
            arguments: json!({}),
        }],
        None,
    );
    fixture.insert(
        &fixture.session,
        Role::Assistant,
        "ambiguous two".into(),
        vec![ToolCall {
            id: "duplicate".into(),
            name: "js".into(),
            arguments: json!({}),
        }],
        None,
    );
    let error = fixture
        .registry
        .get_host("history_read")
        .unwrap()
        .invoke(
            fixture.context(&fixture.session),
            json!({"tool_call_id":"duplicate"}),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("ambiguous"));
    let index = fixture
        .registry
        .get_host("history_chapters")
        .unwrap()
        .invoke(fixture.context(&fork.id), json!({}))
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&index.content).unwrap()["chapters"],
        json!([])
    );
    let fork_messages = fixture.store.messages(&fork.id).unwrap();
    let lookup = fixture
        .registry
        .get_host("history_read")
        .unwrap()
        .invoke(
            fixture.context(&fork.id),
            json!({"message_id":fork_messages[0].id}),
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&lookup.content).unwrap()["messages"][0]["text"],
        "original local"
    );
    assert!(
        fixture
            .registry
            .get_host("history_read")
            .unwrap()
            .invoke(
                fixture.context(&fork.id),
                json!({"episode":batch.episode.id})
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn original_detail_and_tool_arguments_are_searchable_in_history_order() {
    let fixture = Fixture::new();
    let first = fixture.message(&format!(
        "{} ExactIdentifier ÖMEGA {}",
        "prefix ".repeat(100),
        "suffix ".repeat(100)
    ));
    let call = ToolCall {
        id: "call-1".into(),
        name: "name-is-not-searchable".into(),
        arguments: json!({"project":"ExactIdentifier", "choice":"ÖMEGA"}),
    };
    let second = fixture.insert(
        &fixture.session,
        Role::Assistant,
        "Other context".into(),
        vec![call.clone()],
        None,
    );
    fixture.chapter(1, &first, &second, "Original decisions");
    let tail = fixture.insert(
        &fixture.session,
        Role::Tool,
        "ExactIdentifier but another choice".into(),
        vec![],
        Some("call-1"),
    );
    let before = fixture.store.storage_stats().unwrap().transaction_count;
    let result = fixture
        .call(
            "history_search",
            json!({"query":"  exactidentifier\tömega  "}),
        )
        .await;
    assert_eq!(result["matches"].as_array().unwrap().len(), 2);
    assert_eq!(result["matches"][0]["index"], 0);
    assert_eq!(result["matches"][1]["index"], 1);
    assert_eq!(result["matches"][1]["chapter"], 1);
    assert_eq!(result["more"], false);
    assert!(
        result["matches"][0]["snippet"]
            .as_str()
            .unwrap()
            .contains("ExactIdentifier")
    );
    assert!(
        result["matches"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["snippet"].as_str().unwrap().chars().count() <= 300)
    );
    assert_eq!(
        fixture
            .call("history_search", json!({"query":"name-is-not-searchable"}))
            .await["matches"],
        json!([])
    );
    assert_eq!(
        fixture
            .call("history_search", json!({"query":"choice", "chapter":1}))
            .await["matches"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // Argument values are searched as written, not as JSON-escaped text.
    assert_eq!(
        fixture.store.storage_stats().unwrap().transaction_count,
        before
    );
    fixture.insert(
        &fixture.session,
        Role::Assistant,
        String::new(),
        vec![ToolCall {
            id: "call-2".into(),
            name: "js".into(),
            arguments: json!({"code":r#"tools.read({path: "C:\Projects\BubblePaw"})"#}),
        }],
        None,
    );
    let before = fixture.store.storage_stats().unwrap().transaction_count;
    let found = fixture
        .call(
            "history_search",
            json!({"query":r#"C:\Projects\BubblePaw"#}),
        )
        .await;
    assert_eq!(found["matches"].as_array().unwrap().len(), 1);
    let found = fixture
        .call("history_search", json!({"query":r#"{path: "C:\Projects"#}))
        .await;
    assert_eq!(found["matches"].as_array().unwrap().len(), 1);
    let read = fixture
        .call("history_read", json!({"chapter":1,"count":50}))
        .await;
    assert_eq!(read["messages"].as_array().unwrap().len(), 2);
    assert_eq!(read["messages"][1]["tool_calls"], json!([call]));
    assert_eq!(read["next"], Value::Null);
    let linked = fixture.call("history_read", json!({"from":2})).await;
    assert_eq!(linked["messages"][0]["tool_call_id"], "call-1");
    assert_eq!(linked["messages"][0]["text"], tail.content);
    assert_eq!(
        fixture.store.storage_stats().unwrap().transaction_count,
        before
    );
}

#[tokio::test]
async fn chapter_index_includes_all_titles_and_legacy_cutoff_without_summaries() {
    let fixture = Fixture::new();
    let legacy = fixture.message("Legacy exact detail");
    let one = fixture.message("One");
    let two = fixture.message("Two");
    fixture
        .store
        .save_context_checkpoint(&ContextCheckpoint {
            id: "legacy".into(),
            session_id: fixture.session.clone(),
            through_message_id: legacy.id,
            model: "old".into(),
            summary: "Old rolling summary".into(),
            files_touched: vec![],
            created_at: legacy.created_at,
            usage: None,
        })
        .unwrap();
    let first_id = fixture.chapter(1, &one, &one, "First chapter");
    let second_id = fixture.chapter(2, &two, &two, "Second chapter");
    assert_eq!(
        fixture.call("history_chapters", json!({})).await,
        json!({"chapters":[{"id":first_id,"kind":"legacy","status":"legacy","ordinal":1,"title":"First chapter","description":"First chapter","from":1,"to":1},{"id":second_id,"kind":"legacy","status":"legacy","ordinal":2,"title":"Second chapter","description":"Second chapter","from":2,"to":2}],"legacy_cutoff":0})
    );
    assert_eq!(
        fixture.call("history_read", json!({"chapter":2})).await["messages"][0]["index"],
        2
    );
    assert_eq!(
        fixture
            .call("history_search", json!({"query":"legacy"}))
            .await["matches"][0]["chapter"],
        Value::Null
    );
}

#[tokio::test]
async fn defaults_limits_and_character_truncation_preserve_pagination() {
    let fixture = Fixture::new();
    for index in 0..55 {
        fixture.message(&format!("match {index} {}", "🦊".repeat(4100)));
    }
    let defaults = fixture.call("history_read", json!({"from":0})).await;
    // Output budgeting may make pages smaller than the requested count.
    let default_len = defaults["messages"].as_array().unwrap().len();
    assert!(default_len > 0 && default_len <= 20);
    assert_eq!(defaults["next"], default_len);
    let mut next = 0;
    while next < 55 {
        let result = fixture
            .call("history_read", json!({"from":next,"count":100}))
            .await;
        let entries = result["messages"].as_array().unwrap();
        assert!(!entries.is_empty() && entries.len() <= 50);
        for entry in entries {
            assert_eq!(entry["index"], next);
            assert!(entry["text"].as_str().unwrap().chars().count() <= 4000);
            assert_eq!(entry["truncated"], true);
            next += 1;
        }
        assert_eq!(
            result["next"],
            if next < 55 { json!(next) } else { Value::Null }
        );
    }
    let short = Fixture::new();
    for _ in 0..55 {
        short.message("match");
    }
    assert_eq!(
        short
            .call("history_read", json!({"from":0,"count":100}))
            .await["messages"]
            .as_array()
            .unwrap()
            .len(),
        50
    );
    let defaults = short.call("history_search", json!({"query":"MATCH"})).await;
    assert_eq!(defaults["matches"].as_array().unwrap().len(), 20);
    assert_eq!(defaults["more"], true);
    let maximum = short
        .call("history_search", json!({"query":"match","limit":100}))
        .await;
    assert_eq!(maximum["matches"].as_array().unwrap().len(), 50);
    assert_eq!(maximum["more"], true);
    let final_page = short.call("history_read", json!({"from":50})).await;
    assert_eq!(final_page["messages"].as_array().unwrap().len(), 5);
    assert_eq!(final_page["next"], Value::Null);
}

#[tokio::test]
async fn invalid_arguments_and_session_isolation_are_enforced_without_dispatcher() {
    let fixture = Fixture::new();
    fixture.message("Local detail");
    let foreign = fixture.store.create_session("foreign").unwrap().id;
    fixture.insert(&foreign, Role::User, "FOREIGN ONLY".into(), vec![], None);
    let invalid = [
        ("history_chapters", json!({"session_id":foreign})),
        ("history_chapters", json!([])),
        ("history_search", json!({"query":" \t\n "})),
        ("history_search", json!({"query":12})),
        ("history_search", json!({"query":"local","limit":0})),
        ("history_search", json!({"query":"local","limit":-1})),
        ("history_search", json!({"query":"local","chapter":0})),
        ("history_search", json!({"query":"local","chapter":1})),
        ("history_read", json!({})),
        ("history_read", json!({"from":-1})),
        ("history_read", json!({"from":1})),
        ("history_read", json!({"from":0.5})),
        ("history_read", json!({"from":"0"})),
        ("history_read", json!({"from":null})),
        ("history_read", json!({"from":0,"count":0})),
        ("history_read", json!({"chapter":foreign})),
    ];
    for (name, input) in invalid {
        assert!(
            fixture
                .registry
                .get_host(name)
                .unwrap()
                .invoke(fixture.context(&fixture.session), input.clone())
                .await
                .is_err(),
            "accepted {name} {input}"
        );
    }
    assert_eq!(
        fixture
            .call("history_search", json!({"query":"foreign"}))
            .await["matches"],
        json!([])
    );
    let foreign_result = fixture
        .registry
        .get_host("history_read")
        .unwrap()
        .invoke(fixture.context(&foreign), json!({"from":0}))
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&foreign_result.content).unwrap()["messages"][0]["text"],
        "FOREIGN ONLY"
    );
    let unknown = fixture
        .registry
        .get_host("history_read")
        .unwrap()
        .invoke(fixture.context("unknown"), json!({"from":0}))
        .await
        .unwrap_err();
    assert!(unknown.to_string().contains("session"));
    let cancelled = fixture.context(&fixture.session);
    cancelled.cancellation.cancel();
    assert!(
        fixture
            .registry
            .get_host("history_read")
            .unwrap()
            .invoke(cancelled, json!({"from":0}))
            .await
            .is_err()
    );
    let empty = Fixture::new();
    assert_eq!(
        empty.call("history_read", json!({"from":0})).await,
        json!({"messages":[],"next":null})
    );
}
#[path = "history_tools/chapter_compat.rs"]
mod chapter_compat;
#[path = "history_tools/js_read_only.rs"]
mod js_read_only;
