use anyhow::Result;
use async_trait::async_trait;
use bubblepaw_core::{
    CancellationToken, DecisionRequest, DecisionUsage, Harness, HarnessConfig, ModelRequest,
    ModelResponse, Provider, ReasoningEffort, RunOptions, Store, TextCallback, ToolRegistry, Usage,
};
use rusqlite::{Connection, params};
use serde_json::json;
use std::sync::Arc;

#[test]
fn reasoning_effort_serde_and_legacy_request_defaults() -> Result<()> {
    for effort in [
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::XHigh,
        ReasoningEffort::Max,
    ] {
        let encoded = serde_json::to_value(effort)?;
        assert_eq!(encoded, effort.as_str());
        assert_eq!(serde_json::from_value::<ReasoningEffort>(encoded)?, effort);
        let request = ModelRequest {
            reasoning_effort: Some(effort),
            ..Default::default()
        };
        assert_eq!(
            serde_json::from_value::<ModelRequest>(serde_json::to_value(request)?)?
                .reasoning_effort,
            Some(effort)
        );
    }
    assert!(serde_json::from_value::<ReasoningEffort>(json!("disabled")).is_err());
    let mut legacy = serde_json::to_value(ModelRequest::default())?;
    assert!(legacy.get("reasoning_effort").is_none());
    let model: ModelRequest = serde_json::from_value(legacy.clone())?;
    assert_eq!(model.reasoning_effort, None);
    legacy["reasoning_effort"] = json!(null);
    assert_eq!(
        serde_json::from_value::<ModelRequest>(legacy)?.reasoning_effort,
        None
    );
    let decision: DecisionRequest = serde_json::from_value(json!({
        "model": "test", "input": "question", "questions": [], "session_id": null
    }))?;
    assert_eq!(decision.reasoning_effort, ReasoningEffort::Low);
    assert_eq!(
        DecisionRequest::default().reasoning_effort,
        ReasoningEffort::Low
    );
    assert_eq!(serde_json::to_value(decision)?["reasoning_effort"], "low");
    Ok(())
}

#[test]
fn legacy_usage_absent_and_null_cache_read_become_zero() -> Result<()> {
    for legacy in [
        json!({"input_tokens": 21, "output_tokens": 4}),
        json!({"input_tokens": 21, "output_tokens": 4, "cache_read_tokens": null}),
    ] {
        let usage: Usage = serde_json::from_value(legacy)?;
        assert_eq!(usage.input_tokens, 21);
        assert_eq!(usage.output_tokens, 4);
        assert_eq!(usage.cache_read_tokens, 0);
        assert_eq!(usage.cache_creation_tokens, 0);
    }
    let old_nonzero: Usage = serde_json::from_value(json!({
        "input_tokens": 21, "output_tokens": 4, "cache_read_tokens": 8
    }))?;
    assert_eq!(old_nonzero.cache_read_tokens, 8);
    assert_eq!(old_nonzero.cache_creation_tokens, 0);
    Ok(())
}

#[test]
fn legacy_physical_usage_without_output_stays_unknown() -> Result<()> {
    let last: bubblepaw_core::LastRequestUsage =
        serde_json::from_value(json!({"input_tokens": 21}))?;
    assert_eq!(last.input_tokens, Some(21));
    assert_eq!(last.output_tokens, None);
    assert!(serde_json::to_value(last)?.get("output_tokens").is_none());
    Ok(())
}

#[test]
fn usage_roundtrip_accumulation_saturation_and_decision_conversion() -> Result<()> {
    let usage = Usage {
        input_tokens: 21,
        output_tokens: 4,
        cache_read_tokens: 8,
        cache_creation_tokens: 3,
        reasoning_tokens: None,
    };
    assert_eq!(
        serde_json::to_value(serde_json::from_value::<Usage>(serde_json::to_value(
            &usage
        )?)?)?,
        serde_json::to_value(&usage)?
    );
    let mut total = Usage::default();
    total.add(&usage);
    total.add(&usage);
    assert_eq!(total.input_tokens, 42);
    assert_eq!(total.output_tokens, 8);
    assert_eq!(total.cache_read_tokens, 16);
    assert_eq!(total.cache_creation_tokens, 6);
    total.add(&Usage {
        input_tokens: u64::MAX,
        output_tokens: u64::MAX,
        cache_read_tokens: u64::MAX,
        cache_creation_tokens: u64::MAX,
        reasoning_tokens: None,
    });
    assert_eq!(total.input_tokens, u64::MAX);
    assert_eq!(total.output_tokens, u64::MAX);
    assert_eq!(total.cache_read_tokens, u64::MAX);
    assert_eq!(total.cache_creation_tokens, u64::MAX);
    let decision = DecisionUsage::from(&usage);
    assert_eq!(decision.input_tokens, 21);
    assert_eq!(decision.output_tokens, 4);
    let from_decision = Usage::from(decision);
    assert_eq!(from_decision.input_tokens, 21);
    assert_eq!(from_decision.output_tokens, 4);
    // Cache tokens survive the round trip so decider calls can be cache-accounted.
    assert_eq!(from_decision.cache_read_tokens, 8);
    assert_eq!(from_decision.cache_creation_tokens, 3);
    Ok(())
}

struct CachedUsageProvider;
#[async_trait]
impl Provider for CachedUsageProvider {
    async fn generate(
        &self,
        _: ModelRequest,
        _: TextCallback,
        _: CancellationToken,
    ) -> Result<ModelResponse> {
        Ok(ModelResponse {
            text: "persisted answer".into(),
            usage: Some(Usage {
                input_tokens: 21,
                output_tokens: 4,
                cache_read_tokens: 8,
                cache_creation_tokens: 3,
                reasoning_tokens: None,
            }),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn sqlite_completed_run_usage_reads_legacy_and_new_rows_after_reopen() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("usage.sqlite");
    let store = Store::open(&path)?;
    let session = store.create_session("cached usage")?;
    let harness = Harness::new(
        store.clone(),
        Arc::new(CachedUsageProvider),
        ToolRegistry::new(),
        HarnessConfig::default(),
    );
    for _ in 0..3 {
        harness
            .prompt(&session.id, "input", RunOptions::default())
            .await?;
    }
    let runs = store.runs(&session.id)?;
    assert_eq!(runs.len(), 3);
    assert!(runs.iter().all(|run| run.status == "completed"));
    drop(harness);
    drop(store);
    // Recreate the exact old on-disk JSON representations in two completed rows.
    let connection = Connection::open(Store::legacy_data_dir(&path).join("serving.db"))?;
    for (run, legacy_cache) in runs.iter().take(2).zip([None, Some(json!(null))]) {
        let raw: String =
            connection.query_row("SELECT data FROM runs WHERE id=?1", [&run.id], |row| {
                row.get(0)
            })?;
        let mut data: serde_json::Value = serde_json::from_str(&raw)?;
        assert_eq!(data["usage"]["cache_read_tokens"], 8);
        assert_eq!(data["usage"]["cache_creation_tokens"], 3);
        let legacy = data["usage"].as_object_mut().unwrap();
        legacy.remove("cache_creation_tokens");
        legacy.remove("cache_read_tokens");
        if let Some(value) = legacy_cache {
            legacy.insert("cache_read_tokens".into(), value);
        }
        connection.execute(
            "UPDATE runs SET data=?2 WHERE id=?1",
            params![run.id, serde_json::to_string(&data)?],
        )?;
    }
    drop(connection);
    let reopened = Store::open(&path)?;
    for run in &runs[..2] {
        let usage = reopened.run(&run.id)?.usage.unwrap();
        assert_eq!(usage.input_tokens, 21);
        assert_eq!(usage.output_tokens, 4);
        assert_eq!(usage.cache_read_tokens, 0);
        assert_eq!(usage.cache_creation_tokens, 0);
    }
    let new = reopened.run(&runs[2].id)?.usage.unwrap();
    assert_eq!(new.input_tokens, 21);
    assert_eq!(new.output_tokens, 4);
    assert_eq!(new.cache_read_tokens, 8);
    assert_eq!(new.cache_creation_tokens, 3);
    assert_eq!(reopened.runs(&session.id)?.len(), 3);
    assert_eq!(reopened.storage_stats()?.chat_transaction_count, 0);
    Ok(())
}

#[test]
fn reasoning_tokens_are_omitted_when_unreported_and_sum_when_reported() -> Result<()> {
    let usage = Usage {
        input_tokens: 21,
        output_tokens: 4,
        cache_read_tokens: 8,
        cache_creation_tokens: 3,
        reasoning_tokens: None,
    };
    let encoded = serde_json::to_value(&usage)?;
    assert!(encoded.get("reasoning_tokens").is_none());
    let decoded: Usage = serde_json::from_value(encoded)?;
    assert_eq!(decoded.reasoning_tokens, None);

    let mut total = Usage::default();
    total.add(&Usage {
        reasoning_tokens: Some(3),
        ..Default::default()
    });
    total.add(&Usage {
        reasoning_tokens: Some(4),
        ..Default::default()
    });
    assert_eq!(total.reasoning_tokens, Some(7));

    let mut partial = Usage {
        reasoning_tokens: Some(3),
        ..Default::default()
    };
    partial.add(&Usage::default());
    assert_eq!(partial.reasoning_tokens, Some(3));

    let mut none = Usage::default();
    none.add(&Usage::default());
    assert_eq!(none.reasoning_tokens, None);
    Ok(())
}
