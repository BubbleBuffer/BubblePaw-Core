//! Current memory and skills are rebound only together with a successful apply.
use super::*;
use bubblepaw_core::ContextSurfaceRebind;
use serde::{Deserialize, Serialize};

const INITIAL_BINDING: &str = "bubblepaw-context-prompt-v1";
fn binding_key(session: &str) -> String {
    format!("context_prompt_binding:{session}")
}

#[derive(Serialize, Deserialize)]
struct PromptBinding {
    base: String,
    prefix_bytes: usize,
    prefix_sha256: String,
}
impl PromptBinding {
    fn new(base: String, prefix: &str) -> Self {
        Self {
            base,
            prefix_bytes: prefix.len(),
            prefix_sha256: bubblepaw_core::fingerprint(prefix.as_bytes()),
        }
    }
    fn matches(&self, prompt: &str) -> bool {
        prompt.get(..self.prefix_bytes).is_some_and(|prefix| {
            bubblepaw_core::fingerprint(prefix.as_bytes()) == self.prefix_sha256
        })
    }
}

impl LiveConfig {
    pub(super) fn bind_context_prompt(
        &self,
        session: &str,
        base: &str,
        prefix: &str,
    ) -> Result<()> {
        if self.store.attachment(session, INITIAL_BINDING)?.is_none() {
            self.store.store_attachment(
                session,
                INITIAL_BINDING,
                "{\"kind\":\"context-prompt\",\"version\":1}",
                &serde_json::to_vec(&PromptBinding::new(base.into(), prefix))?,
            )?;
        }
        Ok(())
    }

    /// The callback prepares metadata and bytes; publishing belongs to the
    /// summarizer's existing validated view transaction.
    pub fn surface_refresh(self: &Arc<Self>) -> crate::summarization::SurfaceRefresh {
        let weak = Arc::downgrade(self);
        Arc::new(move |session, prompt| match weak.upgrade() {
            Some(live) => live.context_surface_rebind(session, prompt),
            None => Ok(None),
        })
    }

    fn context_surface_rebind(
        &self,
        session: &str,
        prompt: &str,
    ) -> Result<Option<ContextSurfaceRebind>> {
        let binding = match self.store.chat_metadata(session, &binding_key(session))? {
            Some(encoded) => serde_json::from_str::<PromptBinding>(&encoded)?,
            None => match self.store.attachment(session, INITIAL_BINDING)? {
                Some((_, bytes)) => {
                    let initial: PromptBinding = serde_json::from_slice(&bytes)?;
                    // A request-template preview may have resolved a different
                    // base before the actual first run selected a custom one.
                    if initial.matches(prompt) {
                        initial
                    } else {
                        self.derive_prompt_binding(session, prompt)?
                    }
                }
                None => self.derive_prompt_binding(session, prompt)?,
            },
        };
        let saved_prefix = prompt.get(..binding.prefix_bytes).ok_or_else(|| {
            anyhow!(
                "Cannot refresh context: saved host prefix does not match the bound chat prompt"
            )
        })?;
        anyhow::ensure!(
            bubblepaw_core::fingerprint(saved_prefix.as_bytes()) == binding.prefix_sha256,
            "Cannot refresh context: saved host prefix does not match the bound chat prompt"
        );
        let suffix = &prompt[binding.prefix_bytes..];
        self.memory.check(session)?;
        self.check_skills(session, &self.providers.hub().skills())?;
        let mut through = 0;
        self.store.for_each_message_header(session, |seq, _| {
            through = seq;
            Ok(())
        })?;
        let (memory, memory_metadata) = self.memory.compaction_snapshot(session, through)?;
        let (skills, skills_metadata) = self.skills.compaction_snapshot(session, through)?;
        let mut prefix = binding.base.clone();
        for component in [memory, skills] {
            if !component.is_empty() {
                prefix.push_str("\n\n");
                prefix.push_str(&component);
            }
        }
        let system_prompt = format!("{prefix}{suffix}");
        let mut metadata = memory_metadata
            .into_iter()
            .chain(skills_metadata)
            .collect::<Vec<_>>();
        metadata.push((
            binding_key(session),
            serde_json::to_string(&PromptBinding::new(binding.base, &prefix))?,
        ));
        Ok(Some(ContextSurfaceRebind {
            expected_system_prompt: prompt.into(),
            system_prompt,
            metadata,
        }))
    }

    fn derive_prompt_binding(&self, session: &str, prompt: &str) -> Result<PromptBinding> {
        self.memory.check(session)?;
        self.check_skills(session, &self.providers.hub().skills())?;
        let memory = self.memory.system_prompt(session, "")?;
        let component = self.skills.system_prompt(session, &memory);
        if component.is_empty() {
            return Ok(PromptBinding::new(prompt.into(), prompt));
        }
        let matches = prompt
            .match_indices(&component)
            .map(|(start, _)| start)
            .collect::<Vec<_>>();
        anyhow::ensure!(
            matches.len() == 1,
            "Cannot refresh legacy context: initial memory/skills component is missing or ambiguous"
        );
        let start = matches[0];
        Ok(PromptBinding::new(
            prompt[..start].into(),
            &prompt[..start + component.len()],
        ))
    }
}
