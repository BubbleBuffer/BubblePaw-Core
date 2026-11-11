//! Replay report scoring, storage measurement and markdown rendering.
use super::*;

/// Filesystem-only observation avoids opening SQLite or checkpointing a live replay.
pub(super) fn physical_storage(data_dir: &Path) -> Result<Value> {
    let bytes = |path: &Path| std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let serving_bytes = bytes(&data_dir.join("serving.db"));
    let catalog_bytes = bytes(&data_dir.join("catalog.db"));
    let mut chat_bytes = 0;
    let mut wal_bytes =
        bytes(&data_dir.join("serving.db-wal")) + bytes(&data_dir.join("catalog.db-wal"));
    let chats = data_dir.join("chats");
    if chats.exists() {
        for entry in std::fs::read_dir(chats)? {
            let path = entry?.path();
            if path.extension().is_some_and(|extension| extension == "db") {
                chat_bytes += bytes(&path);
                wal_bytes += bytes(&PathBuf::from(format!("{}-wal", path.display())));
            }
        }
    }
    Ok(
        json!({"database_bytes": chat_bytes + serving_bytes + catalog_bytes,
        "chat_bytes": chat_bytes, "serving_bytes": serving_bytes,
        "catalog_bytes": catalog_bytes, "wal_bytes": wal_bytes,
        "basis": "Physical layout artifacts; filesystem stats only, no database reopening"}),
    )
}

pub(super) fn projected_group(calls: &[Value], kinds: &[&str]) -> Value {
    let matching: Vec<_> = calls
        .iter()
        .filter(|call| {
            call["kind"]
                .as_str()
                .is_some_and(|kind| kinds.contains(&kind))
        })
        .collect();
    json!({
        "calls": matching.len(),
        "projected_cost_usd": matching
            .iter()
            .filter_map(|call| call["projected_paid_cost_usd"].as_f64().or_else(|| call["cost_usd"].as_f64()))
            .sum::<f64>(),
    })
}

pub(super) fn token_count(text: &str) -> Result<usize> {
    // bpe_for_tokenizer uses tiktoken-rs's process-wide cached singleton.
    Ok(
        tiktoken_rs::bpe_for_tokenizer(tiktoken_rs::tokenizer::Tokenizer::O200kBase)?
            .encode_ordinary(text)
            .len(),
    )
}
pub(super) fn role_input_tokens(system: &str, user: &str) -> Result<u64> {
    Ok(token_count(&json!({"model":"deepseek-v4.1-flash","messages":[{"role":"system","content":system},{"role":"user","content":user}],"max_tokens":65536,"stream":false,"reasoning_effort":"low","thinking":{"type":"enabled"}}).to_string())? as u64)
}
pub(super) fn call_totals(calls: &[Value]) -> Value {
    json!({"calls":calls.len(),"input_tokens":calls.iter().filter_map(|call|call["input_tokens"].as_u64()).sum::<u64>(),"output_tokens":calls.iter().filter_map(|call|call["output_tokens"].as_u64()).sum::<u64>(),"cache_read_tokens":calls.iter().filter_map(|call|call["cache_read_tokens"].as_u64()).sum::<u64>(),"cache_creation_tokens":calls.iter().filter_map(|call|call["cache_creation_tokens"].as_u64()).sum::<u64>(),"cost_usd":calls.iter().filter_map(|call|call["cost_usd"].as_f64()).sum::<f64>(),"projected_paid_cost_usd":calls.iter().filter_map(|call|call["projected_paid_cost_usd"].as_f64().or_else(||call["cost_usd"].as_f64())).sum::<f64>()})
}

pub(super) fn accepted_boundaries(
    decisions: &[bubblepaw_core::ContextDecision],
    history: &[Message],
) -> Vec<String> {
    let first_typed = history.iter().find(|m| is_typed_user(m)).map(|m| &m.id);
    let positions: HashMap<&String, usize> = history
        .iter()
        .enumerate()
        .map(|(i, m)| (&m.id, i))
        .collect();
    let mut boundaries: Vec<String> = decisions
        .iter()
        .filter_map(|decision| decision.choice.clone())
        .filter(|choice| Some(choice) != first_typed)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    boundaries.sort_by_key(|id| positions.get(id).copied().unwrap_or(usize::MAX));
    boundaries
}
pub(super) fn string_array(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect()
}

fn augment(
    p: usize,
    candidates: &[Vec<usize>],
    assigned: &mut [Option<usize>],
    seen: &mut HashSet<usize>,
) -> bool {
    for &g in &candidates[p] {
        if !seen.insert(g) {
            continue;
        }
        if assigned[g].is_none() || augment(assigned[g].unwrap(), candidates, assigned, seen) {
            assigned[g] = Some(p);
            return true;
        }
    }
    false
}
pub fn boundary_score(
    predicted: &[String],
    gold: &[String],
    ordinals: &HashMap<&str, usize>,
    ambiguous: &[String],
    tolerant: bool,
) -> Value {
    let candidates: Vec<Vec<usize>> = predicted
        .iter()
        .map(|p| {
            let mut eligible: Vec<usize> = gold
                .iter()
                .enumerate()
                .filter(|(_, g)| {
                    p == *g
                        || (tolerant
                            && (ambiguous.contains(p)
                                || ordinals
                                    .get(p.as_str())
                                    .zip(ordinals.get(g.as_str()))
                                    .is_some_and(|(a, b)| a.abs_diff(*b) <= 1)))
                })
                .map(|(i, _)| i)
                .collect();
            eligible.sort_by_key(|&i| {
                (
                    ordinals
                        .get(p.as_str())
                        .zip(ordinals.get(gold[i].as_str()))
                        .map(|(a, b)| a.abs_diff(*b))
                        .unwrap_or(usize::MAX),
                    ordinals
                        .get(gold[i].as_str())
                        .copied()
                        .unwrap_or(usize::MAX),
                )
            });
            eligible
        })
        .collect();
    let mut assigned = vec![None; gold.len()];
    let mut seen_predictions = HashSet::new();
    for (p, id) in predicted.iter().enumerate() {
        if seen_predictions.insert(id) {
            augment(p, &candidates, &mut assigned, &mut HashSet::new());
        }
    }
    let tp = assigned.iter().flatten().count();
    let fp = predicted.len() - tp;
    let missed = gold.len() - tp;
    let matching: Vec<_> = assigned
        .iter()
        .enumerate()
        .filter_map(|(g, p)| p.map(|p| json!({"predicted":predicted[p],"gold":gold[g]})))
        .collect();
    let false_positive: Vec<_> = predicted
        .iter()
        .enumerate()
        .filter(|(i, _)| !assigned.contains(&Some(*i)))
        .map(|(_, p)| p)
        .collect();
    let missed_gold: Vec<_> = gold
        .iter()
        .enumerate()
        .filter(|(i, _)| assigned[*i].is_none())
        .map(|(_, g)| g)
        .collect();
    json!({"tp":tp,"fp":fp,"fn":missed,"precision":if tp+fp>0 {tp as f64/(tp+fp) as f64} else {f64::from(gold.is_empty())},"recall":if gold.is_empty(){1.0}else{tp as f64/gold.len() as f64},"f1":if 2*tp+fp+missed>0{2.0*tp as f64/(2*tp+fp+missed) as f64}else{1.0},"matching":matching,"false":false_positive,"missed":missed_gold})
}

pub(super) fn related_score(
    corpus: &ReplayCorpus,
    history: &[Message],
    episodes: &[bubblepaw_core::ContextEpisode],
    chapters: &[bubblepaw_core::ContextSubjectChapter],
    predicted: &[Value],
) -> Value {
    let membership = |record: &str| -> Option<String> {
        let index = history.iter().position(|m| m.id == record)?;
        episodes
            .iter()
            .find(|e| {
                let first = history.iter().position(|m| m.id == e.first_message_id);
                let last = history.iter().position(|m| m.id == e.last_message_id);
                first
                    .zip(last)
                    .is_some_and(|(first, last)| first <= index && index <= last)
            })
            .map(|e| e.chapter_id.clone())
    };
    let aligned: Vec<Value> = corpus.relates_gold.iter().map(|gold| {
        let source=gold["source_start_record"].as_str().and_then(membership);let target=gold["target_start_record"].as_str().and_then(membership);
        let source_closed=source.as_ref().is_some_and(|id|chapters.iter().any(|c|&c.id==id && c.status==bubblepaw_core::SubjectChapterStatus::Closed));
        let found=source.as_ref().zip(target.as_ref()).is_some_and(|(s,t)|predicted.iter().any(|p|p["source_chapter"]==*s&&p["target_chapter"]==*t));
        json!({"gold":gold,"aligned_source_chapter":source,"aligned_target_chapter":target,"source_closed":source_closed,"found":found})
    }).collect();
    let tp = aligned.iter().filter(|g| g["found"] == true).count();
    let fp = predicted.len().saturating_sub(tp);
    let missed = aligned.len() - tp;
    let gold_chapters = corpus
        .boundary_gold
        .as_ref()
        .and_then(|g| g["chapters"].as_array());
    let mut rows = Vec::new();
    if let Some(gold_chapters) = gold_chapters {
        let labels: HashMap<String, Option<String>> = gold_chapters
            .iter()
            .filter_map(|g| {
                Some((
                    g["id"].as_str()?.to_owned(),
                    g["starts_at"].as_str().and_then(membership),
                ))
            })
            .collect();
        for (i, gold) in gold_chapters.iter().enumerate() {
            let label = gold["id"].as_str().unwrap_or("");
            let source = labels.get(label).and_then(|v| v.as_ref());
            let previous = i
                .checked_sub(1)
                .and_then(|i| gold_chapters[i]["id"].as_str());
            let mut any = HashSet::new();
            let mut found = HashSet::new();
            for link in predicted
                .iter()
                .filter(|p| source.is_some_and(|s| p["source_chapter"] == *s))
            {
                let target = link["target_chapter"].as_str().unwrap_or("");
                let targets: Vec<String> = labels
                    .iter()
                    .filter(|(_, chapter)| chapter.as_deref() == Some(target))
                    .map(|(label, _)| label.clone())
                    .collect();
                let targets = if targets.is_empty() {
                    vec![format!("unmapped:{target}")]
                } else {
                    targets
                };
                for target in targets {
                    any.insert(target.clone());
                    if link["quote_matched"] == true {
                        found.insert(target);
                    }
                }
            }
            let mut any: Vec<_> = any.into_iter().collect();
            any.sort();
            let mut found: Vec<_> = found.into_iter().collect();
            found.sort();
            let expected = gold["relates_to"]
                .as_str()
                .map(|s| vec![s.to_owned()])
                .unwrap_or_default();
            let closed = source.is_some_and(|s| {
                chapters
                    .iter()
                    .any(|c| &c.id == s && c.status == bubblepaw_core::SubjectChapterStatus::Closed)
            });
            rows.push(json!({"chapter":label,"gold":expected,"predicted":any,"quote_found_predicted":found,"previous":previous,"aligned_source_chapter":source,"source_closed":closed,"membership_ambiguous":source.is_some_and(|s|labels.values().filter(|v|v.as_ref()==Some(s)).count()>1)}));
        }
    }
    let full_metrics = json!({"G-any":related_metric(&rows,false,false,false),"G-return":related_metric(&rows,true,false,false)});
    let closed_metrics = json!({"G-any":related_metric(&rows,false,false,true),"G-return":related_metric(&rows,true,false,true)});
    let found_metrics = json!({"G-any":related_metric(&rows,false,true,false),"G-return":related_metric(&rows,true,true,false)});
    json!({"gold":aligned,"predicted":predicted,"tp":tp,"fp":fp,"fn":missed,"precision":if predicted.is_empty(){f64::from(aligned.is_empty())}else{tp as f64/predicted.len() as f64},"recall":if aligned.is_empty(){1.0}else{tp as f64/aligned.len() as f64},"f1":if 2*tp+fp+missed==0{1.0}else{2.0*tp as f64/(2*tp+fp+missed) as f64},"chapter_rows":rows,"C2_metrics":{"full_corpus":full_metrics,"closed_source_only":closed_metrics,"quote_found_only_full_corpus":found_metrics},"basis":"Gold chapter start records mapped to predicted episode membership; merged memberships are flagged and target labels retain every possible gold membership. Full corpus counts unobserved/open source chapters as missed; closed-source-only exposes observable scoring. G-return excludes immediate previous gold chapter from both sets. Quote-found-only is literal matching, not a semantic judge."})
}

fn related_metric(rows: &[Value], returns: bool, found_only: bool, closed_only: bool) -> Value {
    let mut tp = 0;
    let mut fp = 0;
    let mut missed = 0;
    let mut null_rows = 0;
    let mut false_rows = 0;
    let mut count = 0;
    for row in rows
        .iter()
        .filter(|r| !closed_only || r["source_closed"] == true)
    {
        count += 1;
        let previous = row["previous"].as_str();
        let gold: HashSet<String> = string_array(&row["gold"])
            .into_iter()
            .filter(|g| !returns || Some(g.as_str()) != previous)
            .collect();
        let predicted: HashSet<String> = string_array(
            &row[if found_only {
                "quote_found_predicted"
            } else {
                "predicted"
            }],
        )
        .into_iter()
        .filter(|p| !returns || Some(p.as_str()) != previous)
        .collect();
        tp += gold.intersection(&predicted).count();
        fp += predicted.difference(&gold).count();
        missed += gold.difference(&predicted).count();
        if gold.is_empty() {
            null_rows += 1;
            false_rows += usize::from(!predicted.is_empty());
        }
    }
    json!({"rows":count,"tp":tp,"fp":fp,"fn":missed,"precision":if tp+fp>0{Some(tp as f64/(tp+fp)as f64)}else{None},"recall":if tp+missed>0{Some(tp as f64/(tp+missed)as f64)}else{None},"false_link_gold_null_rows":false_rows,"gold_null_rows":null_rows,"false_link_rate":if null_rows>0{Some(false_rows as f64/null_rows as f64)}else{None}})
}

pub(super) fn markdown(report: &Value) -> Result<String> {
    let mut text = format!(
        "# Rust context replay: {}\n\nStrategy: {}; mode: {}; status: {}; messages: {}/{}; threshold: {}.\n\n",
        report["chat"],
        report["context_strategy"],
        report["mode"],
        report["status"],
        report["messages_replayed"],
        report["messages_total"],
        report["threshold"]
    );
    text.push_str(&format!("| Episodes | Chapters | Compactions | Exact F1 | Tolerant F1 | Estimated USD | Input / output tokens |\n|---:|---:|---:|---:|---:|---:|---:|\n| {} | {} | {} | {} | {} | {} | {} / {} |\n\n",report["episodes"].as_array().map_or(0,Vec::len),report["chapters"].as_array().map_or(0,Vec::len),report["compactions"].as_array().map_or(0,Vec::len),report["boundary_scores"]["exact"]["f1"],report["boundary_scores"]["tolerant"]["f1"],report["totals"]["estimated_cost_usd"],report["totals"]["input_tokens"],report["totals"]["output_tokens"]));
    if let Some(groups) = report["probes"]["groups"].as_array() {
        text.push_str(&crate::probes::markdown(groups));
        text.push_str("\nScripted answers and grades prove plumbing only. Recall denominator contains valid judgments; skipped and failed probes remain explicit in JSON.\n\n");
    }
    for (title, key) in [
        (
            "Serving view tokens at typed user turns",
            "view_tokens_per_turn",
        ),
        ("Rolling checkpoint", "rolling_checkpoint"),
        ("Episodes", "episodes"),
        (
            "Decisions (options, averaged probabilities, choice)",
            "decisions",
        ),
        (
            "Chapters (ranges, description, Related quotes and match flags)",
            "chapters",
        ),
        (
            "Compactions (frontier, per-item tokens, masking and saved views)",
            "compactions",
        ),
        ("Boundary gold and exact/tolerant scores", "boundary_scores"),
        (
            "Final serving view (complete replayed history)",
            "final_view",
        ),
        ("Related gold and scores", "related"),
        ("Calls, tokens, peak-price cost and wall time", "totals"),
        (
            "Storage while replay connections are open (DB, WAL, committed transactions)",
            "storage",
        ),
        (
            "Retained storage after replay connections close",
            "final_artifact_storage",
        ),
        ("Transaction accounting", "storage_transaction_basis"),
        ("Failures and stop reason", "failures"),
        ("Corpus and label provenance", "provenance"),
        ("Unverified and limitations", "limitations"),
    ] {
        text.push_str(&format!(
            "## {title}\n\n```json\n{}\n```\n\n",
            serde_json::to_string_pretty(&report[key])?
        ));
    }
    if !report["stop_reason"].is_null() {
        text.push_str(&format!("Stop reason: {}\n", report["stop_reason"]));
    }
    Ok(text)
}
