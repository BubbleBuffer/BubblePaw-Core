//! Skill folder discovery and bounded YAML metadata parsing.
use super::{Catalog, MAX_SKILL_BYTES, PREFIX, Skill, TRUNCATED, catalog_line};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    io::Read,
    path::{Path, PathBuf},
};

const MAX_FRONTMATTER: usize = 64 * 1024;

pub(super) fn discover(
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

pub(super) fn load(files: &[PathBuf]) -> Result<Catalog> {
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
        digest: bubblepaw_core::hex_lower(hash.finalize()),
    })
}
