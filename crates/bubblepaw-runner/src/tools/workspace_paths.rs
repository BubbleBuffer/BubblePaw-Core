use super::*;

impl WorkspaceTools {
    pub(super) fn readable(
        &self,
        relative: &str,
        session: &str,
        data_dir: Option<&Path>,
    ) -> Result<(PathBuf, bool)> {
        let path = self
            .root
            .join(relative)
            .canonicalize()
            .with_context(|| format!("read path does not exist: {relative}"))?;
        let data_dir = self
            .spill_data_dir
            .as_deref()
            .or(data_dir)
            .unwrap_or(&self.root);
        let spill_root = data_dir.join("spill");
        if path.starts_with(&spill_root) || Path::new(relative).starts_with(&spill_root) {
            let spills = crate::spill::SpillStore::new(data_dir)?;
            return Ok((spills.readable(session, &self.root.join(relative))?, true));
        }
        if path.starts_with(&self.root) {
            return Ok((path, false));
        }
        let roots = self
            .skill_read_roots
            .read()
            .map_err(|_| anyhow!("skill read roots poisoned"))?;
        ensure!(
            roots
                .get(session)
                .is_some_and(|roots| roots.iter().any(|root| path.starts_with(root))),
            "path escapes workspace and this session's read grants"
        );
        Ok((path, false))
    }
    pub(crate) fn existing(&self, relative: &str) -> Result<PathBuf> {
        let resolved = fs::canonicalize(self.root.join(relative))
            .with_context(|| format!("workspace path does not exist: {relative}"))?;
        ensure!(
            resolved.starts_with(&self.root),
            "path escapes workspace root"
        );
        Ok(resolved)
    }
    pub(super) fn destination(&self, relative: &str) -> Result<PathBuf> {
        let path = self.root.join(relative);
        if let Some(scratch) = &self.scratch_boundary {
            scratch
                .resolve(&path, true)?
                .ok_or_else(|| anyhow!("path escapes scratchpad"))?;
        }
        let mut ancestor = path
            .parent()
            .ok_or_else(|| anyhow!("path needs a parent"))?;
        let mut missing = Vec::new();
        while !ancestor.exists() {
            ensure!(
                fs::symlink_metadata(ancestor).is_err(),
                "destination parent is inaccessible or a dangling symlink"
            );
            missing.push(
                ancestor
                    .file_name()
                    .ok_or_else(|| anyhow!("invalid destination parent"))?
                    .to_owned(),
            );
            ancestor = ancestor
                .parent()
                .ok_or_else(|| anyhow!("path needs an existing ancestor"))?;
        }
        let mut parent = fs::canonicalize(ancestor)?;
        ensure!(
            parent.starts_with(&self.root) && parent.is_dir(),
            "path escapes workspace root"
        );
        for component in missing.into_iter().rev() {
            parent.push(component);
        }
        let destination = parent.join(
            path.file_name()
                .ok_or_else(|| anyhow!("path needs a filename"))?,
        );
        ensure!(
            !fs::symlink_metadata(&destination).is_ok_and(|m| m.file_type().is_symlink()),
            "refusing to replace a symlink"
        );
        if destination.exists() {
            self.existing(relative)
        } else {
            Ok(destination)
        }
    }
}
