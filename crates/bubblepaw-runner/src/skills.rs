//! Host-owned skill catalogs. Bodies remain ordinary files, never prompt attachments.
use anyhow::{Context, Result};
use bubblepaw_core::{MessageOrigin, Store};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Mutex,
};

use crate::live_config::{FileStamp, file_stamps};

#[path = "skills_discovery.rs"]
mod discovery;
use discovery::{discover, load};

pub const MAX_SKILL_BYTES: usize = 8 * 1024;
const ATTACHMENT: &str = "bubblepaw-skills-v1";
fn binding_key(session: &str) -> String {
    format!("skills_snapshot:{session}")
}

#[derive(Serialize, Deserialize)]
struct FrozenCatalog {
    through: i64,
    catalog: Catalog,
    read_roots: Vec<PathBuf>,
}
const NOTICE: &str = "Skills changed (the session system catalog stays frozen):\n";
const PREFIX: &str = "Available skills (frozen at session start). Read a skill's SKILL.md with the ordinary read tool when its description matches the task. Paths are absolute; supporting files in each listed skill folder are also readable.\n";
const TRUNCATED: &str = "[Skill catalog truncated at 8 KiB; remaining skills omitted.]\n";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Skill {
    name: String,
    description: String,
    path: PathBuf,
    /// Includes the body, so body-only edits are announced without loading it into context.
    digest: String,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Catalog {
    skills: Vec<Skill>,
    warnings: Vec<String>,
    truncated: bool,
}
impl Catalog {
    fn prompt(&self) -> String {
        if self.skills.is_empty() && !self.truncated {
            return String::new();
        }
        let mut prompt = PREFIX.to_owned();
        for skill in &self.skills {
            prompt.push_str(&catalog_line(skill));
        }
        if self.truncated {
            prompt.push_str(TRUNCATED);
        }
        prompt
    }
    fn apply(&mut self, delta: Delta) {
        self.skills
            .retain(|skill| !delta.removed.contains(&skill.name));
        for skill in delta.updated {
            if let Some(old) = self.skills.iter_mut().find(|old| old.name == skill.name) {
                *old = skill;
            } else {
                self.skills.push(skill);
            }
        }
        self.warnings = delta.warnings;
    }
}
#[derive(Serialize, Deserialize)]
struct Delta {
    added: Vec<String>,
    removed: Vec<String>,
    changed: Vec<String>,
    updated: Vec<Skill>,
    warnings: Vec<String>,
}
struct SessionSkills {
    frozen: Catalog,
    observed: Catalog,
    read_roots: HashSet<PathBuf>,
    stamps: Vec<FileStamp>,
    pending: Vec<String>,
    binding: Option<String>,
}

pub struct Skills {
    store: Store,
    workspace: PathBuf,
    user_directory: Option<PathBuf>,
    sessions: Mutex<HashMap<String, SessionSkills>>,
}
impl Skills {
    pub fn new(store: Store, workspace: PathBuf, user_directory: Option<PathBuf>) -> Self {
        Self {
            store,
            workspace,
            user_directory,
            sessions: Mutex::default(),
        }
    }

    /// Uses the same mtime/size helper and turn boundary as repository memory files.
    /// Directory stamps and candidate paths also catch additions and removals.
    pub fn check(&self, session: &str, enabled: bool, directories: &[PathBuf]) -> Result<()> {
        let (paths, files) = if enabled {
            let workspace = crate::worktrees::workspace(&self.store, session, &self.workspace)?;
            discover(&workspace, self.user_directory.as_deref(), directories)?
        } else {
            (Vec::new(), Vec::new())
        };
        let stamps = file_stamps(paths)?;
        let mut sessions = self.sessions.lock().unwrap();
        let binding = self.store.chat_metadata(session, &binding_key(session))?;
        if sessions
            .get(session)
            .is_some_and(|state| state.binding != binding)
        {
            sessions.remove(session);
        }
        if let Some(state) = sessions.get_mut(session) {
            if state.stamps != stamps {
                let current = load(&files)?;
                state.pending.extend(changes(&state.observed, &current)?);
                state.read_roots.extend(roots(&current));
                state.observed = current;
                state.stamps = stamps;
            }
            return Ok(());
        }
        let current = load(&files)?;
        let mut read_roots = HashSet::new();
        let latest = binding
            .as_deref()
            .map(serde_json::from_str::<FrozenCatalog>)
            .transpose()?;
        let (frozen, mut observed, mut pending) =
            match self.store.attachment(session, ATTACHMENT)? {
                Some((_, bytes)) => {
                    let original: Catalog =
                        serde_json::from_slice(&bytes).context("Invalid saved skill catalog")?;
                    read_roots.extend(roots(&original));
                    let (frozen, through) = match latest {
                        Some(latest) => {
                            read_roots.extend(latest.read_roots);
                            (latest.catalog, latest.through)
                        }
                        None => (original, 0),
                    };
                    let mut observed = frozen.clone();
                    // Initial warnings are delivered through notices, not the frozen prompt.
                    // Retry them after interruption until a durable notice records delivery.
                    if through == 0 {
                        observed.warnings.clear();
                    }
                    self.store.for_each_message_range(
                        session,
                        through.saturating_add(1),
                        i64::MAX,
                        |_, message| {
                            if message.origin == Some(MessageOrigin::Generated) {
                                for change in message.content.split(NOTICE).skip(1) {
                                    if let Some(Ok(delta)) =
                                        serde_json::Deserializer::from_str(change)
                                            .into_iter::<Delta>()
                                            .next()
                                    {
                                        read_roots.extend(delta.updated.iter().filter_map(
                                            |skill| skill.path.parent().map(Path::to_path_buf),
                                        ));
                                        observed.apply(delta);
                                    }
                                }
                            }
                            Ok(())
                        },
                    )?;
                    (frozen, observed, Vec::new())
                }
                None => {
                    // Persist even an empty catalog: initial absence also stays frozen on reopen.
                    self.store.store_attachment(
                        session,
                        ATTACHMENT,
                        "{\"kind\":\"skills\",\"version\":1}",
                        &serde_json::to_vec(&current)?,
                    )?;
                    let pending = if current.warnings.is_empty() {
                        Vec::new()
                    } else {
                        changes(
                            &Catalog::default(),
                            &Catalog {
                                warnings: current.warnings.clone(),
                                ..Default::default()
                            },
                        )?
                    };
                    (current.clone(), current.clone(), pending)
                }
            };
        pending.extend(changes(&observed, &current)?);
        read_roots.extend(roots(&frozen));
        read_roots.extend(roots(&current));
        observed = current;
        sessions.insert(
            session.into(),
            SessionSkills {
                frozen,
                observed,
                read_roots,
                stamps,
                pending,
                binding,
            },
        );
        Ok(())
    }

    pub fn system_prompt(&self, session: &str, base: &str) -> String {
        let sessions = self.sessions.lock().unwrap();
        match sessions
            .get(session)
            .map(|state| state.frozen.prompt())
            .filter(|prompt| !prompt.is_empty())
        {
            Some(prompt) => format!("{base}\n\n{prompt}"),
            None => base.to_owned(),
        }
    }

    pub fn initialized(&self, session: &str) -> bool {
        self.sessions.lock().unwrap().contains_key(session)
    }

    /// Keep the frozen catalog readable as well as skills announced in live notices.
    pub fn read_roots(&self, session: &str) -> Vec<PathBuf> {
        let sessions = self.sessions.lock().unwrap();
        let mut roots: Vec<_> = sessions
            .get(session)
            .map(|state| state.read_roots.iter().cloned().collect())
            .unwrap_or_default();
        roots.sort();
        roots
    }

    pub fn take_changes(&self, session: &str) -> Vec<String> {
        let committed = self.store.chat_metadata(session, &binding_key(session));
        self.sessions
            .lock()
            .unwrap()
            .get_mut(session)
            .map(|state| {
                if committed
                    .as_ref()
                    .is_ok_and(|binding| *binding != state.binding)
                {
                    state.pending.clear();
                }
                std::mem::take(&mut state.pending)
            })
            .unwrap_or_default()
    }

    pub(crate) fn compaction_snapshot(
        &self,
        session: &str,
        through: i64,
    ) -> Result<(String, Option<(String, String)>)> {
        let sessions = self.sessions.lock().unwrap();
        let Some(state) = sessions.get(session) else {
            return Ok((String::new(), None));
        };
        let mut read_roots = state.read_roots.iter().cloned().collect::<Vec<_>>();
        read_roots.sort();
        Ok((
            state.observed.prompt(),
            Some((
                binding_key(session),
                serde_json::to_string(&FrozenCatalog {
                    through,
                    catalog: state.observed.clone(),
                    read_roots,
                })?,
            )),
        ))
    }
}

fn roots(catalog: &Catalog) -> impl Iterator<Item = PathBuf> + '_ {
    catalog
        .skills
        .iter()
        .filter_map(|skill| skill.path.parent().map(Path::to_path_buf))
}

fn catalog_line(skill: &Skill) -> String {
    // JSON quoting prevents a description or path from impersonating another entry.
    format!(
        "- {}\n",
        serde_json::json!({"name":skill.name,"description":skill.description,"path":skill.path})
    )
}

fn changes(before: &Catalog, after: &Catalog) -> Result<Vec<String>> {
    let updated: Vec<_> = after
        .skills
        .iter()
        .filter(|skill| !before.skills.contains(skill))
        .cloned()
        .collect();
    let added = updated
        .iter()
        .filter(|skill| !before.skills.iter().any(|old| old.name == skill.name))
        .map(|skill| skill.name.clone())
        .collect::<Vec<_>>();
    let changed = updated
        .iter()
        .filter(|skill| before.skills.iter().any(|old| old.name == skill.name))
        .map(|skill| skill.name.clone())
        .collect::<Vec<_>>();
    let removed = before
        .skills
        .iter()
        .filter(|skill| !after.skills.iter().any(|new| new.name == skill.name))
        .map(|skill| skill.name.clone())
        .collect::<Vec<_>>();
    if updated.is_empty() && removed.is_empty() && before.warnings == after.warnings {
        return Ok(Vec::new());
    }
    Ok(vec![format!(
        "{NOTICE}{}",
        serde_json::to_string(&Delta {
            added,
            changed,
            removed,
            updated,
            warnings: after.warnings.clone()
        })?
    )])
}

#[cfg(test)]
mod compaction_tests {
    use super::*;

    #[test]
    fn applied_catalog_keeps_original_snapshot_and_external_read_grants() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let external = tempfile::tempdir()?;
        let folder = external.path().join("removed");
        std::fs::create_dir_all(&folder)?;
        let canonical_folder = folder.canonicalize()?;
        let path = folder.join("SKILL.md");
        std::fs::write(
            &path,
            "---\nname: original\ndescription: Original external skill\n---\nbody",
        )?;
        let store = Store::in_memory()?;
        let session = store.create_session("skill snapshot")?.id;
        let skills = Skills::new(store.clone(), workspace.path().into(), None);
        let dirs = [external.path().to_owned()];
        skills.check(&session, true, &dirs)?;
        let original = store.attachment(&session, ATTACHMENT)?;
        assert!(
            skills
                .system_prompt(&session, "base")
                .contains("Original external skill")
        );
        std::fs::remove_file(&path)?;
        skills.check(&session, true, &dirs)?;
        let (prompt, metadata) = skills.compaction_snapshot(&session, 0)?;
        assert!(prompt.is_empty());
        let (key, value) = metadata.unwrap();
        store.set_chat_metadata(&key, &value)?;
        assert!(
            skills.take_changes(&session).is_empty(),
            "same-turn tool rounds must not requeue absorbed changes"
        );
        skills.check(&session, true, &dirs)?;
        assert_eq!(skills.system_prompt(&session, "base"), "base");
        assert!(skills.take_changes(&session).is_empty());
        assert!(skills.read_roots(&session).contains(&canonical_folder));
        assert_eq!(store.attachment(&session, ATTACHMENT)?, original);
        let reopened = Skills::new(store, workspace.path().into(), None);
        reopened.check(&session, true, &dirs)?;
        assert!(reopened.take_changes(&session).is_empty());
        assert!(reopened.read_roots(&session).contains(&canonical_folder));
        Ok(())
    }
}
