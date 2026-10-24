//! Provider replay provenance travels beside opaque state, never inside artifacts.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const PROVIDER_STATE_ORIGIN_FIELD: &str = "_bubblepaw_origin";

/// Chat reasoning replays within the current tool loop by default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningReplay {
    None,
    #[default]
    ToolLoop,
    All,
}

impl std::str::FromStr for ReasoningReplay {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "none" => Ok(Self::None),
            "tool_loop" => Ok(Self::ToolLoop),
            "all" => Ok(Self::All),
            _ => bail!("reasoning_replay must be none, tool_loop or all"),
        }
    }
}

/// Add provenance without modifying a native artifact or replacing prior origin.
/// Model IDs are the adapter's exact request model, without a provider alias.
pub fn with_provider_state_origin(
    mut state: Value,
    provider_kind: &str,
    model: Option<&str>,
    source: &str,
) -> Result<Value> {
    ensure!(
        !provider_kind.is_empty() && !source.is_empty(),
        "Provider state origin needs kind and source"
    );
    let object = state
        .as_object_mut()
        .context("Provider state envelope must be an object")?;
    object
        .entry(PROVIDER_STATE_ORIGIN_FIELD)
        .or_insert_with(|| {
            json!({
                "provider_kind": provider_kind,
                "model": model.filter(|model| !model.is_empty()),
                "source": source,
            })
        });
    Ok(state)
}

/// A replay filter, never a storage filter. Legacy originless native states keep
/// their prior adapter behavior. Known foreign kinds/models are not put on wire.
pub fn provider_state_replay_compatible(state: &Value, provider_kind: &str, model: &str) -> bool {
    if state["adapter"] != provider_kind {
        return false;
    }
    let Some(origin) = state.get(PROVIDER_STATE_ORIGIN_FIELD) else {
        return true;
    };
    if origin["provider_kind"] != provider_kind {
        return false;
    }
    match origin.get("model") {
        None | Some(Value::Null) => true,
        Some(Value::String(origin_model)) => origin_model == model,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_replay_defaults_to_tool_loop_with_explicit_overrides() -> Result<()> {
        assert_eq!(ReasoningReplay::default(), ReasoningReplay::ToolLoop);
        for (name, policy) in [
            ("none", ReasoningReplay::None),
            ("tool_loop", ReasoningReplay::ToolLoop),
            ("all", ReasoningReplay::All),
        ] {
            assert_eq!(name.parse::<ReasoningReplay>()?, policy);
            assert_eq!(
                serde_json::from_value::<ReasoningReplay>(json!(name))?,
                policy
            );
            assert_eq!(serde_json::to_value(policy)?, json!(name));
        }
        Ok(())
    }

    #[test]
    fn origin_never_changes_artifact_or_prior_metadata() -> Result<()> {
        let opaque = json!({"encrypted_content":"opaque+/=雪","id":"reason","unknown":{"n":-0.0}});
        let state = with_provider_state_origin(
            json!({"adapter":"responses","items":[opaque.clone()]}),
            "responses",
            Some("model-a"),
            "native",
        )?;
        assert_eq!(
            serde_json::to_vec(&state["items"][0])?,
            serde_json::to_vec(&opaque)?
        );
        assert_eq!(
            with_provider_state_origin(state.clone(), "responses", None, "import:codex")?,
            state
        );
        assert!(provider_state_replay_compatible(
            &state,
            "responses",
            "model-a"
        ));
        assert!(!provider_state_replay_compatible(
            &state,
            "responses",
            "model-b"
        ));
        assert!(!provider_state_replay_compatible(
            &state,
            "anthropic-messages",
            "model-a"
        ));
        assert!(provider_state_replay_compatible(
            &json!({"adapter":"responses"}),
            "responses",
            "legacy-model"
        ));
        Ok(())
    }
}
