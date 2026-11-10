//! Live summary-fork cache check, one test per route; ordinary runs make no model calls.
//!
//! Run only after authorizing paid calls:
//! `cargo test -p bubblepaw-runner --test provider_live_forks -- --ignored --test-threads=1`
//!
//! Each test sends a short main request that calls a tool, the main request
//! continued after the tool result (which writes the prompt cache), then a
//! summary fork of that capture, and asserts the fork read cached input.
//! - anthropic_messages: ANTHROPIC_API_KEY; claude-haiku-5-5 (override with
//!   BP_LIVE_ANTHROPIC_MESSAGES_MODEL). Cacheable minimum assumed 4096 tokens,
//!   so the frozen prompt is about 4.8k tokens: three calls, about 15k input in
//!   total plus a few hundred output tokens.
//! - responses: OPENAI_API_KEY; gpt-6-luna (override with BP_LIVE_RESPONSES_MODEL).
//!   Cacheable minimum 1024 tokens; the prompt is about 1.5k tokens: three
//!   calls, about 4.5k input in total.
//! - codex: BP_LIVE_CODEX=1 and ~/.codex/auth.json; gpt-6-luna (override with
//!   BP_LIVE_CODEX_MODEL). Same sizes as responses. This is also the only check
//!   that the subscription backend accepts a system item in mid-input.
use anyhow::{Result, ensure};
use bubblepaw_core::{
    CacheTiers, CancellationToken, Message, MessageOrigin, ModelRequest, Provider, ReasoningEffort,
    Role, ToolDefinition,
};
use bubblepaw_runner::provider::{MessagesProvider, ResponsesProvider};
use serde_json::json;
use std::{path::PathBuf, sync::Arc};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn message(id: &str, role: Role, content: &str) -> Message {
    let origin = (role == Role::User).then_some(MessageOrigin::Typed);
    Message {
        id: id.into(),
        session_id: "live-fork".into(),
        role,
        origin,
        content: content.into(),
        tool_calls: vec![],
        tool_call_id: None,
        provider_state: None,
        is_error: false,
        created_at: String::new(),
    }
}

/// About `sentences` * 16 tokens of stable instructions.
fn frozen_prompt(sentences: usize) -> String {
    let mut prompt = String::from(
        "You are a terse assistant. When asked for a sum, call the add tool exactly once.\n",
    );
    for n in 0..sentences {
        prompt.push_str(&format!(
            "Rule {n}: keep answers short and never mention rule {n}.\n"
        ));
    }
    prompt
}

async fn fork_reads_the_cache(
    provider: Arc<dyn Provider>,
    model: &str,
    sentences: usize,
) -> Result<()> {
    let mut main = ModelRequest {
        model: model.into(),
        system_prompt: frozen_prompt(sentences),
        messages: vec![message("u", Role::User, "What is 17 plus 25?")],
        tools: vec![ToolDefinition {
            name: "add".into(),
            description: "Return the integer sum of a and b.".into(),
            input_schema: json!({"type":"object","additionalProperties":false,"properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"]}),
            read_only: true,
            approval: Default::default(),
        }],
        reasoning_effort: Some(ReasoningEffort::Low),
        session_id: "live-fork".into(),
        ..Default::default()
    };
    let first = provider
        .generate(main.clone(), Arc::new(|_| {}), CancellationToken::new())
        .await?;
    ensure!(!first.tool_calls.is_empty(), "the model did not call add");
    let mut assistant = message("a", Role::Assistant, &first.text);
    assistant.tool_calls = first.tool_calls.clone();
    assistant.provider_state = first.provider_state.clone();
    let mut tool = message("t", Role::Tool, "42");
    tool.tool_call_id = Some(first.tool_calls[0].id.clone());
    main.messages.extend([assistant, tool]);
    // The continued main request writes the cache entry the fork must read.
    provider
        .generate(main.clone(), Arc::new(|_| {}), CancellationToken::new())
        .await?;
    let cut = main.messages.len();
    let mut fork = main;
    let mut control = message("", Role::System, "EPISODE MEMORY BRANCH (host-generated).");
    control.origin = None;
    let mut ret = message("", Role::User, "HOST-GENERATED SUMMARY RETURN CONTROL.");
    ret.origin = Some(MessageOrigin::Generated);
    fork.messages.extend([control, ret]);
    fork.session_id.clear();
    fork.cache_tiers = Some(CacheTiers {
        cold_end: None,
        previous_len: Some(cut),
    });
    let reply = provider
        .generate_summary_fork(fork, CancellationToken::new())
        .await?;
    let usage = reply.usage.expect("fork usage");
    ensure!(
        usage.cache_read_tokens > 0,
        "the fork read no cached input: input={} read={} written={}",
        usage.input_tokens,
        usage.cache_read_tokens,
        usage.cache_creation_tokens
    );
    Ok(())
}

#[tokio::test]
#[ignore = "live: paid model calls"]
async fn anthropic_messages_fork_reads_cache() -> Result<()> {
    let Some(key) = env("ANTHROPIC_API_KEY") else {
        println!("skipped: ANTHROPIC_API_KEY not set");
        return Ok(());
    };
    let model =
        env("BP_LIVE_ANTHROPIC_MESSAGES_MODEL").unwrap_or_else(|| "claude-haiku-5-5".into());
    let provider = MessagesProvider::api_key(key, "https://api.anthropic.com/v1")?;
    fork_reads_the_cache(Arc::new(provider), &model, 300).await
}

#[tokio::test]
#[ignore = "live: paid model calls"]
async fn responses_fork_reads_cache() -> Result<()> {
    let Some(key) = env("OPENAI_API_KEY") else {
        println!("skipped: OPENAI_API_KEY not set");
        return Ok(());
    };
    let model = env("BP_LIVE_RESPONSES_MODEL").unwrap_or_else(|| "gpt-6-luna".into());
    let provider = ResponsesProvider::api_key(key, "https://api.openai.com/v1")?;
    fork_reads_the_cache(Arc::new(provider), &model, 95).await
}

#[tokio::test]
#[ignore = "live: paid model calls"]
async fn codex_fork_reads_cache() -> Result<()> {
    let auth = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|home| PathBuf::from(home).join(".codex/auth.json"))
        .filter(|path| path.is_file());
    let (true, Some(auth)) = (env("BP_LIVE_CODEX").as_deref() == Some("1"), auth) else {
        println!("skipped: BP_LIVE_CODEX=1 or ~/.codex/auth.json missing");
        return Ok(());
    };
    let model = env("BP_LIVE_CODEX_MODEL").unwrap_or_else(|| "gpt-6-luna".into());
    let provider = ResponsesProvider::codex_auth(auth, "https://chatgpt.com/backend-api/codex")?;
    fork_reads_the_cache(Arc::new(provider), &model, 95).await
}
