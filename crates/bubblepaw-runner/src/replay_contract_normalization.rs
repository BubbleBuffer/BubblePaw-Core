//! Scripted replay contract normalization; production input is unchanged.
use super::*;

fn replay_view_blocks(
    calculator: &SummarizingContext,
    report: &Value,
    canonical: &[Message],
    requests: &[(usize, ModelRequest)],
) -> Option<(
    Vec<bubblepaw_core::ContextBlock>,
    Vec<bubblepaw_core::ContextBlock>,
    u64,
)> {
    let mut value = report["durable_serving_view"].clone();
    if value["blocks_estimated"].as_bool() != Some(true) {
        return None;
    }
    // Creation times are ignored by this oracle and removed by normalize.
    // Restore them only in cloned values for typed deserialization.
    value["created_at"] = json!("2026-10-10T00:00:00Z");
    let view: bubblepaw_core::ContextView = serde_json::from_value(value).unwrap();
    let episodes = report["episodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            let mut value = row["episode"].clone();
            value["created_at"] = json!("2026-10-10T00:00:00Z");
            serde_json::from_value(value).unwrap()
        })
        .collect::<Vec<bubblepaw_core::ContextEpisode>>();
    let chapters = serde_json::from_value::<Vec<bubblepaw_core::ContextSubjectChapter>>(
        report["chapters"].clone(),
    )
    .unwrap();
    let step = report["compactions"].as_array().unwrap().last().unwrap()["step"]
        .as_u64()
        .unwrap() as usize;
    let request = &requests.iter().find(|(at, _)| *at == step).unwrap().1;
    let rebuilt = calculator
        .episode_view_blocks_for_test(&view, request, &canonical[..step], &episodes, &chapters)
        .unwrap();
    Some((view.blocks, rebuilt, view.tokens as u64))
}

pub(super) fn normalized_replay_contract(
    mut report: Value,
    canonical_ids: &HashSet<String>,
    canonical: &[Message],
) -> Value {
    let calculator = SummarizingContext::new(
        Store::in_memory().unwrap(),
        Arc::new(ScriptedProvider::new("No oracle model calls")),
        default_replay_summary_config(),
        None,
    )
    .unwrap();
    let raw_prepared: Vec<(usize, ModelRequest)> =
        serde_json::from_value(report["prepared_requests"].clone()).unwrap();
    let raw_sizes: Vec<_> = raw_prepared
        .iter()
        .map(|(_, request)| calculator.estimated_tokens(request))
        .collect();
    let mut canonical_prefix = vec![0_usize];
    for message in canonical {
        canonical_prefix
            .push(canonical_prefix.last().unwrap() + calculator.message_tokens(message));
    }
    let initial = calculator.estimated_tokens(&ModelRequest {
        system_prompt: "Keep fixture facts".into(),
        ..ModelRequest::default()
    });
    let mut generated = HashMap::<String, String>::new();
    if let Some(session) = report["session_id"].as_str() {
        generated.insert(session.into(), "generated-session".into());
    }
    for (field, nested) in [
        ("episodes", Some("episode")),
        ("chapters", None),
        ("decisions", None),
    ] {
        if let Some(rows) = report[field].as_array() {
            for (index, row) in rows.iter().enumerate() {
                let object = nested.map_or(row, |key| &row[key]);
                if let Some(id) = object["id"].as_str() {
                    generated.insert(id.into(), format!("generated-{field}-{}", index + 1));
                }
            }
        }
    }
    if let Some(id) = report["rolling_checkpoint"]["id"].as_str() {
        generated.insert(id.into(), "generated-checkpoint".into());
    }
    fn collect_messages(
        value: &Value,
        canonical: &HashSet<String>,
        generated: &mut HashMap<String, String>,
    ) {
        if let Some(object) = value.as_object() {
            if object.contains_key("role")
                && object.contains_key("content")
                && let Some(id) = object.get("id").and_then(Value::as_str)
                && !canonical.contains(id)
                && !generated.contains_key(id)
            {
                let marker = format!("generated-serving-message-{}", generated.len());
                generated.insert(id.into(), marker);
            }
            for child in object.values() {
                collect_messages(child, canonical, generated);
            }
        } else if let Some(array) = value.as_array() {
            for child in array {
                collect_messages(child, canonical, generated);
            }
        }
    }
    collect_messages(&report, canonical_ids, &mut generated);
    fn normalize(value: &mut Value, generated: &HashMap<String, String>) {
        match value {
            Value::Object(object) => {
                // Only generated runtime timestamps/request hashes and measured
                // timings vary. Canonical timestamps are checked separately below.
                if object.contains_key("role") && object.contains_key("content") {
                    object.insert("created_at".into(), json!("2026-10-10T00:00:00Z"));
                } else {
                    object.remove("created_at");
                }
                for key in [
                    "wall_seconds",
                    "elapsed_seconds",
                    "latency_ms",
                    "frozen_view_sha256",
                ] {
                    object.remove(key);
                }
                for child in object.values_mut() {
                    normalize(child, generated);
                }
            }
            Value::Array(array) => {
                for child in array {
                    normalize(child, generated);
                }
            }
            Value::String(text) => {
                for (id, marker) in generated {
                    *text = text.replace(id, marker);
                }
            }
            _ => {}
        }
    }
    for field in [
        "storage",
        "storage_transaction_basis",
        "phase_timings",
        "archive_index",
    ] {
        report.as_object_mut().unwrap().remove(field);
    }
    if let Some((stored, rebuilt, tokens)) =
        replay_view_blocks(&calculator, &report, canonical, &raw_prepared)
    {
        assert_eq!(
            stored, rebuilt,
            "Saved blocks must match their exact raw input spans"
        );
        assert_eq!(
            stored.iter().map(|block| block.tokens).sum::<u64>() + 8,
            tokens
        );
    }
    normalize(&mut report, &generated);
    let normalized_prepared: Vec<(usize, ModelRequest)> =
        serde_json::from_value(report["prepared_requests"].clone()).unwrap();
    let normalized_sizes: Vec<_> = normalized_prepared
        .iter()
        .map(|(_, request)| calculator.estimated_tokens(request))
        .collect();
    if let Some((_, rebuilt, _)) =
        replay_view_blocks(&calculator, &report, canonical, &normalized_prepared)
    {
        report["durable_serving_view"]["blocks"] = serde_json::to_value(rebuilt).unwrap();
    }
    let mut anchors = Vec::new();
    if let Some(compactions) = report["compactions"].as_array_mut() {
        for compaction in compactions {
            let step = compaction["step"].as_u64().unwrap() as usize;
            let index = raw_prepared
                .iter()
                .position(|(prepared_step, _)| *prepared_step == step)
                .unwrap();
            assert_eq!(
                compaction["tokens"], raw_sizes[index],
                "Compaction metric must equal its exact raw prepared request estimate"
            );
            anchors.push((step, raw_sizes[index], normalized_sizes[index]));
            compaction["tokens"] = json!(normalized_sizes[index]);
        }
    }
    // UUID text affects BPE, even after equivalent views are constructed.
    // Validate every runtime count against its raw request/canonical suffix,
    // then compare the exact same estimator after normalizing only generated IDs.
    for (field, token_key) in [
        ("view_tokens_per_turn", "tokens"),
        ("turn_frontiers", "view_tokens"),
    ] {
        if let Some(rows) = report[field].as_array_mut() {
            for row in rows {
                let step = row["step"].as_u64().unwrap() as usize;
                let (anchor_step, raw, normalized) = anchors
                    .iter()
                    .rev()
                    .copied()
                    .find(|(anchor_step, _, _)| *anchor_step <= step)
                    .unwrap_or((0, initial, initial));
                let suffix = canonical_prefix[step] - canonical_prefix[anchor_step];
                assert_eq!(
                    row[token_key],
                    raw + suffix,
                    "Recorded tokens at {field} step {step} must match raw canonical accounting"
                );
                row[token_key] = json!(normalized + suffix);
            }
        }
    }
    if let Some(mismatches) = report["probes"]["plan_mismatches"].as_array_mut() {
        for mismatch in mismatches {
            let row = &mut mismatch["actual"];
            let step = row["step"].as_u64().unwrap() as usize;
            let (anchor_step, raw, normalized) = anchors
                .iter()
                .rev()
                .copied()
                .find(|(anchor_step, _, _)| *anchor_step <= step)
                .unwrap_or((0, initial, initial));
            let suffix = canonical_prefix[step] - canonical_prefix[anchor_step];
            assert_eq!(
                row["view_tokens"],
                raw + suffix,
                "Mismatch snapshot must retain exact raw token accounting"
            );
            row["view_tokens"] = json!(normalized + suffix);
        }
    }
    if report["durable_serving_view"]["tokens"].is_number() {
        let (_, raw, normalized) = anchors.last().copied().unwrap();
        assert_eq!(report["durable_serving_view"]["tokens"], raw);
        report["durable_serving_view"]["tokens"] = json!(normalized);
    }
    if report["final_view"]["tokens"].is_number() {
        assert_eq!(report["final_view"]["tokens"], *raw_sizes.last().unwrap());
        report["final_view"]["tokens"] = json!(*normalized_sizes.last().unwrap());
    }
    report
}
