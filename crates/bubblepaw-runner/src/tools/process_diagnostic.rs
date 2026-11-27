use serde_json::{Value, json};

/// A bounded explanation shared by exec/poll/wait. Raw stdout stays in its
/// existing capture fields; only exceptional outcomes need this extra context.
pub(super) fn process_diagnostic(value: &Value) -> Option<String> {
    let status = value["status"].as_str().unwrap_or("exited");
    let exit = value["exitCode"].as_i64();
    let failed = matches!(status, "timed_out" | "cancelled" | "timeout")
        || (status == "exited" && exit != Some(0));
    let mut capture_notes = Vec::new();
    let mut decoding_notes = Vec::new();
    let complete_spill = value.get("fullOutput").is_some();
    for stream in ["stdout", "stderr"] {
        let capture = &value["capture"][stream];
        let omitted: u64 = capture["gaps"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|gap| {
                gap["to"]
                    .as_u64()
                    .unwrap_or(0)
                    .saturating_sub(gap["from"].as_u64().unwrap_or(0))
            })
            .sum();
        if capture["lossyUtf8"] == true {
            decoding_notes.push(format!("{stream}: decoding is lossy (invalid UTF-8)"));
        }
        if (omitted > 0 && !complete_spill) || capture["captureError"] == true {
            capture_notes.push(format!(
                "{stream}: {} total bytes, {omitted} omitted bytes{}",
                capture["totalBytes"].as_u64().unwrap_or(0),
                if capture["captureError"] == true {
                    "; pipe capture error"
                } else {
                    ""
                }
            ));
        }
    }
    if !failed && capture_notes.is_empty() && decoding_notes.is_empty() && !complete_spill {
        return None;
    }
    let mut explanation = match status {
        "exited" if exit == Some(0) => "Command succeeded (exit code 0).".to_owned(),
        "exited" => match exit {
            Some(code) => format!("Command exited with code {code}. Check the command and its arguments; nonzero exits can also mean no matches."),
            None => "Command terminated without an exit code.".to_owned(),
        },
        "timed_out" => "Command timed out and was terminated; increase timeoutMs only if appropriate.".to_owned(),
        "cancelled" => "Command was cancelled and terminated.".to_owned(),
        "timeout" => "Wait timed out; the command is still running. Poll or wait again using the same handle.".to_owned(),
        _ => "Command is still running.".to_owned(),
    };
    if complete_spill {
        explanation.push_str(" Displayed output is a head/tail preview; complete decoded output is saved at fullOutput.path. Use read with line ranges.");
    }
    if !capture_notes.is_empty() {
        explanation.push_str(&format!(" Capture is incomplete ({}). Omitted output does not indicate command failure. Request smaller output or batches, or redirect full output to a file and use file({{path}}).", capture_notes.join("; ")));
    }
    if !decoding_notes.is_empty() {
        explanation.push_str(&format!(" {}. The decoded text is altered; do not treat it as exact output or complete JSON. Redirect raw output to a file to preserve the bytes.", decoding_notes.join("; ")));
    }
    if failed {
        let stderr = value["stderr"].as_str().unwrap_or("");
        if !stderr.trim().is_empty() {
            let preview: String = stderr.chars().take(768).collect();
            explanation.push_str(&format!(" stderr: {preview}"));
            if stderr.chars().count() > 768 {
                explanation.push_str(" [stderr preview shortened]");
            }
        }
    }
    Some(explanation)
}

pub(super) fn with_process_diagnostic(mut value: Value) -> Value {
    if let Some(diagnostic) = process_diagnostic(&value) {
        value["diagnostic"] = json!(diagnostic);
    }
    value
}

/// A finished command that did not succeed: a non-zero (or missing) exit code,
/// or a deadline that killed it.
pub(super) fn failed_exit(value: &Value) -> bool {
    match value["status"].as_str() {
        Some("exited") => value["exitCode"].as_i64() != Some(0),
        Some("timed_out") => true,
        _ => false,
    }
}
