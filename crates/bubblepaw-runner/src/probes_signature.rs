//! Plan fingerprints and the pinned strategy sources and prompts.
use anyhow::Result;
use serde::Serialize;
use sha2::{Digest, Sha256};

pub fn fingerprint<T: Serialize>(value: &T) -> Result<String> {
    Ok(bubblepaw_core::hex_lower(Sha256::digest(
        serde_json::to_vec(value)?,
    )))
}
pub fn strategy_signature() -> String {
    // Pin the existing policies and prompts; the planner never modifies them.
    bubblepaw_core::hex_lower(Sha256::digest(concat!(
        include_str!("context_strategies.rs"),
        include_str!("summarization.rs"),
        include_str!("episode_worker.rs"),
        include_str!("episodes.rs"),
        include_str!("episode_models.rs"),
        include_str!("episode_models_fork.rs"),
        include_str!("episode_fork.rs"),
        include_str!("history_snapshot.rs"),
        include_str!("summary_prompt_v2.txt"),
        include_str!("summary_prompt_v3.txt"),
        include_str!("chapter_prompt_v1.txt"),
        include_str!("classical_prompt_v1.txt")
    )))
}
