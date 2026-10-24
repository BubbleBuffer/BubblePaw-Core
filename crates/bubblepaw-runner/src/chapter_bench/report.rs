use super::{Arm, BenchOptions, episodes::FrozenEpisodes, meter::Meter};
use crate::replay::{ReplayCorpus, boundary_score};
use anyhow::Result;
use bubblepaw_core::is_typed_user;
use serde_json::{Value, json};
use std::collections::HashMap;

pub(super) fn score(corpus: &ReplayCorpus, arm: &Arm, selection: super::arms::Selection) -> Value {
    let predicted = selection.predicted_ids;
    let typed: Vec<_> = corpus
        .messages
        .iter()
        .filter(|m| is_typed_user(m))
        .map(|m| m.id.as_str())
        .collect();
    let ordinals: HashMap<_, _> = typed
        .into_iter()
        .enumerate()
        .map(|(i, id)| (id, i))
        .collect();
    let labels = corpus.boundary_gold.as_ref().expect("validated gold");
    let strings = |key: &str| -> Vec<String> {
        labels[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().into())
            .collect()
    };
    let gold = strings("gold");
    let ambiguous = strings("ambiguous");
    json!({"arm":arm.name,"chat":corpus.chat_id,"predicted_ids":predicted,"gold_ids":gold,"probes":selection.probes,
        "exact":boundary_score(&predicted,&gold,&ordinals,&ambiguous,false),
        "tolerant":boundary_score(&predicted,&gold,&ordinals,&ambiguous,true)})
}

pub(super) fn write(
    options: &BenchOptions,
    arms: &[Arm],
    stages: &[(&ReplayCorpus, FrozenEpisodes)],
    results: Vec<Value>,
    errors: Vec<Value>,
    meter: &Meter,
) -> Result<Value> {
    let mut arm_reports = Vec::new();
    for arm in arms {
        let chats: Vec<_> = results
            .iter()
            .filter(|r| r["arm"] == arm.name)
            .cloned()
            .map(|mut r| {
                r["usage"] = meter.totals(|c| {
                    c.invocation == meter.invocation
                        && c.scope.arm == arm.name
                        && c.scope.chat == r["chat"]
                });
                r
            })
            .collect();
        arm_reports.push(
            json!({"name":arm.name,"kind":arm.kind,"model":arm.model,"chats":chats,
            "errors":errors.iter().filter(|error|error["arm"]==arm.name).collect::<Vec<_>>(),
            "totals":meter.totals(|c|c.invocation==meter.invocation && c.scope.arm==arm.name),
            "exact":aggregate(&chats,"exact"),"tolerant":aggregate(&chats,"tolerant")}),
        );
    }
    let stage_ids: Vec<_> = stages.iter().map(|(_, s)| s.stage_id.as_str()).collect();
    let episodes:Vec<_>=stages.iter().map(|(c,s)|json!({"chat":c.chat_id,"stage_id":s.stage_id,
        "episodes":s.episodes.len(),"candidates":s.episodes.len().saturating_sub(1),"uncovered_tail_messages":s.uncovered_tail_messages,
        "usage":meter.totals(|call|call.scope.stage=="episodes" && call.scope.stage_id==s.stage_id)})).collect();
    let value = json!({"version":1,"mode":if options.dry_run {"scripted"}else{"live"},
        "status":if errors.is_empty(){"completed"}else{"incomplete"},"invocation":meter.invocation,
        "max_cost_usd":options.max_cost_usd,"errors":errors,"arms":arm_reports,
        "call_unit":"provider invocation; registry retries and decision repeats count separately; adapter continuations are aggregated",
        "episodes":{"model":super::SUMMARY_MODEL,"chats":episodes,
            "totals":meter.totals(|c|c.scope.stage=="episodes" && (stage_ids.contains(&c.scope.stage_id.as_str()) || c.invocation==meter.invocation))},
        "invocation_totals":meter.totals(|c|c.invocation==meter.invocation),"ledger_totals":meter.totals(|_|true)});
    std::fs::write(
        options.out.join("report.json"),
        serde_json::to_vec_pretty(&value)?,
    )?;
    std::fs::write(options.out.join("report.md"), markdown(&value))?;
    Ok(value)
}
fn aggregate(chats: &[Value], kind: &str) -> Value {
    if chats.is_empty() {
        return json!({"tp":0,"fp":0,"fn":0,"precision":null,"recall":null,"f1":null});
    }
    let sum = |key: &str| {
        chats
            .iter()
            .map(|c| c[kind][key].as_u64().unwrap_or(0))
            .sum::<u64>()
    };
    let (tp, fp, missed) = (sum("tp"), sum("fp"), sum("fn"));
    let ratio = |a: u64, b: u64| if b == 0 { 1.0 } else { a as f64 / b as f64 };
    let precision = if tp + fp == 0 {
        f64::from(tp + missed == 0)
    } else {
        ratio(tp, tp + fp)
    };
    json!({"tp":tp,"fp":fp,"fn":missed,"precision":precision,"recall":ratio(tp,tp+missed),"f1":ratio(2*tp,2*tp+fp+missed)})
}
fn markdown(report: &Value) -> String {
    let mut text = format!(
        "# Chapter selection benchmark\n\nMode: {}. Status: {}. Scores are micro-averaged over completed chats.\n\n",
        report["mode"], report["status"]
    );
    text.push_str("| Arm / stage | Calls | Input | Cached | Output | USD | p50 ms | p95 ms | Errors | Exact P/R/F1 | Tolerant P/R/F1 |\n|---|---:|---:|---:|---:|---:|---:|---:|---:|---|---|\n");
    let mut row = |name: &str, u: &Value, e: &Value, t: &Value| {
        text.push_str(&format!(
            "| {name} | {} | {} | {} | {} | {} | {} | {} | {} | {} / {} / {} | {} / {} / {} |\n",
            u["calls"],
            u["input_tokens"],
            u["cached_tokens"],
            u["output_tokens"],
            u["cost_usd"],
            u["p50_latency_ms"],
            u["p95_latency_ms"],
            u["errors"],
            e["precision"],
            e["recall"],
            e["f1"],
            t["precision"],
            t["recall"],
            t["f1"]
        ));
    };
    row(
        "episode generation",
        &report["episodes"]["totals"],
        &Value::Null,
        &Value::Null,
    );
    for arm in report["arms"].as_array().unwrap() {
        row(
            arm["name"].as_str().unwrap(),
            &arm["totals"],
            &arm["exact"],
            &arm["tolerant"],
        );
    }
    text.push_str("\nEpisode generation includes reused frozen stages; arm totals cover this invocation. Cost cap uses cumulative ledger spend. Unknown costs are not zero and block further calls. Calls and latencies count provider invocations: registry retries and decision repeats separately, adapter continuations aggregated.\n");
    for arm in report["arms"].as_array().unwrap() {
        for chat in arm["chats"].as_array().unwrap() {
            text.push_str(&format!(
                "\n## {} / {}\n\nPredicted IDs: `{}`\n\nExact: `{}`\n\nTolerant: `{}`\n",
                arm["name"], chat["chat"], chat["predicted_ids"], chat["exact"], chat["tolerant"]
            ));
        }
    }
    if report["errors"].as_array().is_some_and(|e| !e.is_empty()) {
        text.push_str(&format!("\nErrors: `{}`\n", report["errors"]));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn evaluated_empty_boundaries_match_replay_scoring_without_scoring_missing_chats() {
        for (predicted, gold) in [
            (vec![], vec![]),
            (vec![], vec!["g"]),
            (vec!["p"], vec![]),
            (vec!["p"], vec!["g"]),
            (vec!["p"], vec!["p"]),
        ] {
            let strings = |ids: Vec<&str>| ids.into_iter().map(str::to_owned).collect::<Vec<_>>();
            let score = boundary_score(
                &strings(predicted),
                &strings(gold),
                &HashMap::new(),
                &[],
                false,
            );
            let total = aggregate(&[json!({"exact":score})], "exact");
            for key in ["precision", "recall", "f1"] {
                assert_eq!(total[key], score[key]);
                assert!(aggregate(&[], "exact")[key].is_null());
            }
        }
    }
}
