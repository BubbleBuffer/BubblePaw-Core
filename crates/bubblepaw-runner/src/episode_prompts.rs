//! Versioned memory prompts.
use serde::{Deserialize, Serialize};

/// Episode summary prompt version. `v3` adds the gist line and the `Actions:` log.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EpisodePrompt {
    #[default]
    V2,
    V3,
    /// The first v3 draft, kept only to re-measure against.
    #[cfg(feature = "bench")]
    #[serde(rename = "v3-draft")]
    V3Draft,
}
impl EpisodePrompt {
    pub fn text(self) -> &'static str {
        match self {
            Self::V2 => include_str!("summary_prompt_v2.txt"),
            Self::V3 => include_str!("summary_prompt_v3.txt"),
            #[cfg(feature = "bench")]
            Self::V3Draft => include_str!("summary_prompt_v3_draft.txt"),
        }
    }
}

/// Chapter synthesis prompt version; v1 remains the ordinary default.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ChapterPrompt {
    #[default]
    V1,
    V2,
}
impl ChapterPrompt {
    pub fn text(self) -> &'static str {
        match self {
            Self::V1 => include_str!("chapter_prompt_v1.txt"),
            Self::V2 => include_str!("chapter_prompt_v2.txt"),
        }
    }
}
