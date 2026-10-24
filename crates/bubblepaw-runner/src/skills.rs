//! Host-owned skill catalogs. Bodies remain ordinary files, never prompt attachments.
use anyhow::{Context, Result, ensure};
use bubblepaw_core::{MessageOrigin, Store};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    path::{Path, PathBuf},
    sync::Mutex,
};

use crate::live_config::{FileStamp, file_stamps};

pub const MAX_SKILL_BYTES: usize = 8 * 1024;
const MAX_FRONTMATTER: usize = 64 * 1024;
const ATTACHMENT: &str = "bubblepaw-skills-v1";
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
        let (frozen, mut observed, mut pending) =
            match self.store.attachment(session, ATTACHMENT)? {
                Some((_, bytes)) => {
                    let frozen: Catalog =
                        serde_json::from_slice(&bytes).context("Invalid saved skill catalog")?;
                    let mut observed = frozen.clone();
                    // Initial warnings are delivered through notices, not the frozen prompt.
                    // Retry them after interruption until a durable notice records delivery.
                    observed.warnings.clear();
                    self.store
                        .for_each_message_range(session, 0, i64::MAX, |_, message| {
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
                        })?;
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
        self.sessions
            .lock()
            .unwrap()
            .get_mut(session)
            .map(|state| std::mem::take(&mut state.pending))
            .unwrap_or_default()
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

fn discover(
    workspace: &Path,
    user: Option<&Path>,
    extra: &[PathBuf],
) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let workspace = workspace
        .canonicalize()
        .context("Skill workspace must exist")?;
    ensure!(workspace.is_dir(), "Skill workspace must be a directory");
    let root = workspace
        .ancestors()
        .find(|directory| directory.join(".git").exists())
        .unwrap_or(&workspace);
    let mut directories = Vec::new();
    for directory in workspace.ancestors() {
        directories.push(directory.join(".agents/skills"));
        directories.push(directory.join(".claude/skills"));
        if directory == root {
            break;
        }
    }
    if let Some(user) = user {
        directories.push(user.to_owned());
    }
    directories.extend_from_slice(extra);
    let mut stamps = Vec::new();
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    for directory in directories {
        if !seen.insert(directory.clone()) {
            continue;
        }
        stamps.push(directory.clone());
        stamps.push(directory.join("SKILL.md"));
        if directory.join("SKILL.md").is_file() {
            files.push(directory.join("SKILL.md"));
            continue;
        }
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("Cannot list skill directory {}", directory.display())
                });
            }
        };
        let mut children = entries
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        children.sort();
        for child in children {
            if child.is_dir() {
                stamps.push(child.clone());
                let file = child.join("SKILL.md");
                stamps.push(file.clone());
                if file.is_file() {
                    files.push(file);
                }
            }
        }
    }
    Ok((stamps, files))
}

fn load(files: &[PathBuf]) -> Result<Catalog> {
    let mut catalog = Catalog::default();
    let mut seen = HashSet::new();
    let mut used = PREFIX.len() + TRUNCATED.len();
    for path in files {
        match load_skill(path) {
            Ok(skill) => {
                if !seen.insert(skill.name.clone()) {
                    continue;
                }
                let size = catalog_line(&skill).len();
                if used + size <= MAX_SKILL_BYTES {
                    used += size;
                    catalog.skills.push(skill);
                } else {
                    catalog.truncated = true;
                }
            }
            Err(_) => {
                // Do not echo YAML parser input: frontmatter is user-controlled content.
                let warning = format!(
                    "Skipped skill {}: invalid or unreadable YAML frontmatter; nonempty string name and description are required",
                    path.display()
                );
                if catalog.warnings.iter().map(String::len).sum::<usize>() + warning.len()
                    < MAX_SKILL_BYTES
                {
                    catalog.warnings.push(warning);
                }
            }
        }
    }
    Ok(catalog)
}

fn load_skill(path: &Path) -> Result<Skill> {
    let folder = path
        .parent()
        .context("Skill needs a folder")?
        .canonicalize()?;
    let path = path.canonicalize()?;
    ensure!(path.starts_with(&folder), "Skill file escapes its folder");
    let mut reader = std::fs::File::open(&path)?;
    let mut hash = Sha256::new();
    let mut header = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        let keep = count.min((MAX_FRONTMATTER + 1).saturating_sub(header.len()));
        header.extend_from_slice(&buffer[..keep]);
    }
    // Only parse the header; the bounded buffer may end midway through a body character.
    let end = header
        .split_inclusive(|byte| *byte == b'\n')
        .scan(0usize, |offset, line| {
            *offset += line.len();
            Some((*offset, line))
        })
        .skip(1)
        .find(|(_, line)| line.trim_ascii_end() == b"---" || line.trim_ascii_end() == b"...")
        .map(|(end, _)| end)
        .context("Unclosed YAML frontmatter")?;
    let header = std::str::from_utf8(&header[..end])?;
    let mut lines = header.split_inclusive('\n');
    ensure!(
        lines.next().is_some_and(|line| line.trim_end() == "---"),
        "Missing YAML frontmatter"
    );
    let mut yaml = String::new();
    let mut closed = false;
    for line in lines {
        if line.trim_end() == "---" || line.trim_end() == "..." {
            closed = true;
            break;
        }
        yaml.push_str(line);
    }
    ensure!(
        closed && yaml.len() <= MAX_FRONTMATTER,
        "Unclosed or oversized YAML frontmatter"
    );
    #[derive(Deserialize)]
    struct Frontmatter {
        name: String,
        description: String,
    }
    let front: Frontmatter = serde_yaml::from_str(&yaml)?;
    let name = front.name.trim().to_owned();
    let description = front.description.trim().to_owned();
    ensure!(
        !name.is_empty() && !description.is_empty() && !name.chars().any(char::is_control),
        "Invalid skill metadata"
    );
    Ok(Skill {
        name,
        description,
        path,
        digest: format!("{:x}", hash.finalize()),
    })
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
