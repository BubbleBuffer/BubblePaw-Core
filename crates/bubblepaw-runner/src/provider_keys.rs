//! Host-owned provider API keys. The settings contain references, never values.
//! Windows inherits the data directory ACL, as the Cloudflare token writer does.
use anyhow::{Result, bail, ensure};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};
use zeroize::Zeroizing;

const LIMIT: usize = 4096;

pub(crate) fn path(data_dir: &Path, name: &str) -> Result<PathBuf> {
    ensure!(
        crate::provider_setup::valid_name(name),
        "Invalid provider name"
    );
    // Hex keeps distinct case-sensitive provider names distinct on Windows too.
    let encoded: String = name.bytes().map(|byte| format!("{byte:02x}")).collect();
    Ok(data_dir
        .join("secrets")
        .join(format!("provider-{encoded}.key")))
}

pub(crate) fn validate(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= LIMIT
            && value.bytes().all(|byte| byte.is_ascii_graphic()),
        "Expected one provider API key (at most 4096 printable ASCII bytes)"
    );
    Ok(())
}

pub(crate) fn read(data_dir: &Path, name: &str) -> Result<Option<Zeroizing<String>>> {
    let path = path(data_dir, name)?;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => bail!("Cannot read stored provider key"),
    };
    let mut value = Zeroizing::new(String::new());
    file.take((LIMIT + 1) as u64)
        .read_to_string(&mut value)
        .map_err(|_| anyhow::anyhow!("Cannot read stored provider key"))?;
    validate(&value)?;
    Ok(Some(value))
}

pub(crate) fn write(data_dir: &Path, name: &str, value: &str) -> Result<()> {
    validate(value)?;
    let path = path(data_dir, name)?;
    let dir = path.parent().expect("provider secret has a parent");
    let result = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let mut file = tempfile::NamedTempFile::new_in(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(value.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(&path).map_err(|error| error.error)?;
        Ok(())
    })();
    result.map_err(|_| anyhow::anyhow!("Cannot write stored provider key"))
}

pub(crate) fn remove(data_dir: &Path, name: &str) -> Result<()> {
    match std::fs::remove_file(path(data_dir, name)?) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => bail!("Cannot remove stored provider key"),
    }
}

impl crate::config::Settings {
    /// Stored keys take precedence over the invoking environment, per route.
    pub(crate) fn provider_key(
        &self,
        name: &str,
        environment: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Option<String>> {
        // The loader historically allows IDs broader than setup's filename
        // allowlist. Those existing routes continue using their environment.
        if crate::provider_setup::valid_name(name)
            && let Some(value) = read(&self.storage.data_dir, name)?
        {
            return Ok(Some(value.to_string()));
        }
        Ok(self
            .providers
            .get(name)
            .and_then(|provider| provider.key_env.as_ref())
            .and_then(|name| environment(name))
            .filter(|value| !value.is_empty()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_atomic_bounded_store_and_safe_names() -> Result<()> {
        let root = tempfile::tempdir()?;
        for name in ["", "../x", "a/b", "a\\b", "C:foo", "..", "a\0b"] {
            assert!(path(root.path(), name).is_err());
        }
        assert_ne!(path(root.path(), "Foo")?, path(root.path(), "foo")?);
        for invalid in ["", "key\nvalue", "key value", "key\0value", "é"] {
            assert!(write(root.path(), "fixture", invalid).is_err());
        }
        assert!(write(root.path(), "fixture", &"a".repeat(LIMIT + 1)).is_err());
        write(root.path(), "fixture", "synthetic-provider-key-marker")?;
        write(root.path(), "fixture", "synthetic-replacement-marker")?;
        assert_eq!(
            read(root.path(), "fixture")?.as_deref().map(String::as_str),
            Some("synthetic-replacement-marker")
        );
        assert_eq!(std::fs::read_dir(root.path().join("secrets"))?.count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path(root.path(), "fixture")?)?
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        std::fs::write(path(root.path(), "fixture")?, "b".repeat(LIMIT + 1))?;
        assert!(read(root.path(), "fixture").is_err());
        remove(root.path(), "fixture")?;
        remove(root.path(), "fixture")?;
        assert!(read(root.path(), "fixture")?.is_none());
        Ok(())
    }

    #[test]
    fn stored_keys_are_route_specific_and_legacy_ids_keep_environment_lookup() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut settings = crate::config::EffectiveConfig::load(crate::config::LoadOptions {
            workspace: root.path().into(),
            home: None,
            custom_file: None,
            env: Vec::new(),
            cli: Vec::new(),
        })?
        .settings()?;
        settings.storage.data_dir = root.path().to_owned();
        let mut provider = settings.providers["opencode"].clone();
        provider.key_env = Some("FIXTURE_API_KEY".into());
        for name in ["fixture", "alias", "legacy+route"] {
            settings.providers.insert(name.into(), provider.clone());
        }
        let environment = |_: &str| Some("synthetic-env-key".into());
        write(root.path(), "fixture", "synthetic-stored-key")?;
        assert_eq!(
            settings.provider_key("fixture", &environment)?.as_deref(),
            Some("synthetic-stored-key")
        );
        for name in ["alias", "legacy+route"] {
            assert_eq!(
                settings.provider_key(name, &environment)?.as_deref(),
                Some("synthetic-env-key")
            );
        }
        remove(root.path(), "fixture")?;
        assert_eq!(
            settings.provider_key("fixture", &environment)?.as_deref(),
            Some("synthetic-env-key")
        );
        Ok(())
    }
}
