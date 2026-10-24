//! Standalone recall judges retain source-session lineage without changing provider input.
use super::*;

pub async fn judge(model: &dyn ProbeModel, probe: &Probe, answer: &str) -> Value {
    judge_for_session(model, probe, answer, "").await
}

pub(crate) async fn judge_for_session(
    model: &dyn ProbeModel,
    probe: &Probe,
    answer: &str,
    session: &str,
) -> Value {
    let messages = vec![
        json!({"role":"system","content":JUDGE_PROMPT}),
        json!({"role":"user","content":json!({"question":probe.question,"expected_answer":probe.expected_answer,"answer":answer}).to_string()}),
    ];
    let response = model
        .call_in_session(request(&messages, &[]), "judge", session)
        .await;
    match response {
        Ok(payload) => {
            let raw = payload_message(&payload)
                .ok()
                .and_then(|item| item["content"].as_str())
                .unwrap_or("");
            match parse_judgment(raw) {
                Ok(judgment) => {
                    json!({"status":"graded","judgment":judgment,"raw":raw,"usage":usage(&payload),"finish_reason":payload["choices"][0]["finish_reason"]})
                }
                Err(_) => {
                    json!({"status":"judge_failed","error":"Judge JSON/schema failure; no retry or fabricated grade","raw":raw,"usage":usage(&payload),"finish_reason":payload["choices"][0]["finish_reason"]})
                }
            }
        }
        Err(_) => {
            json!({"status":"judge_failed","error":"Judge transport/provider failure; no retry or fabricated grade"})
        }
    }
}
