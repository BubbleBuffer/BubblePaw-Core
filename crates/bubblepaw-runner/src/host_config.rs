//! The CLI host's startup configuration as a library entry point. `bubblepaw`
//! layers its configuration and places its store through these functions, and
//! [`HostConfig::load`] adds the provider and role checks the host runs when it
//! starts, so an embedder gets the same providers and model roles as a CLI host
//! on the same data directory.
use crate::{
    cli::Cli,
    config::{EffectiveConfig, LoadOptions, Override, Settings},
    host_attach::PreparedRequest,
    provider::LiveProviderConfig,
    worktrees::CleanupReport,
};
use bubblepaw_core::{Store, StoreOptions};
use clap::Parser;
use std::{
    ffi::OsString,
    fmt,
    path::{Path, PathBuf},
};

/// Where the configuration comes from. Every field left empty behaves like
/// the matching `bubblepaw` default: the current directory, the central data
/// directory, the user and project config files, `USERPROFILE` or `HOME`, and
/// the process environment.
#[derive(Clone, Debug, Default)]
pub struct ConfigOptions {
    /// The workspace (`--workspace`); its `.bubblepaw/config.toml` is the
    /// project config file.
    pub workspace: Option<PathBuf>,
    /// The data directory (`--data-dir`), above `storage.data_dir`.
    pub data_dir: Option<PathBuf>,
    /// One file instead of the user and project files (`--config`).
    pub config_file: Option<PathBuf>,
    /// The home directory holding `.bubblepaw/config.toml`.
    pub home: Option<PathBuf>,
    /// The environment for `BUBBLEPAW_*` keys and credentials.
    pub env: Option<Vec<(String, String)>>,
    /// Ordered key overrides, applied last (`--set`).
    pub overrides: Vec<Override>,
}

/// Why the configuration cannot start a host.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigError {
    /// A config file cannot be read or is not TOML.
    File { path: PathBuf, message: String },
    /// The workspace does not exist or is not a directory.
    Workspace { path: PathBuf, message: String },
    /// A key has an invalid value.
    Settings(String),
    /// A model role does not resolve to qualified models.
    Roles(String),
    /// The provider setup or the main model's route is invalid.
    Providers(String),
    /// The data directory cannot be placed.
    Storage(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File { message, .. } | Self::Workspace { message, .. } => f.write_str(message),
            Self::Settings(message)
            | Self::Roles(message)
            | Self::Providers(message)
            | Self::Storage(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ConfigError {}

/// A role resolution failure inside the settings, kept as the error's own
/// type so [`ConfigError::Roles`] can be told apart.
#[derive(Debug)]
pub(crate) struct RoleError(String);

impl fmt::Display for RoleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RoleError {}

pub(crate) fn role_error(error: anyhow::Error) -> anyhow::Error {
    if error.chain().any(|cause| cause.is::<RoleError>()) {
        return error;
    }
    RoleError(format!("{error:#}")).into()
}

/// `other` unless the failure is a role error.
fn classify(error: anyhow::Error, other: fn(String) -> ConfigError) -> ConfigError {
    let message = format!("{error:#}");
    if error.chain().any(|cause| cause.is::<RoleError>()) {
        ConfigError::Roles(message)
    } else {
        other(message)
    }
}

/// Layer the configuration the way `bubblepaw` does: defaults, the config
/// files, the environment and `options.overrides`, with relative paths
/// anchored at the current directory. The settings are checked, providers
/// are not.
pub fn layer(options: &ConfigOptions) -> Result<EffectiveConfig, ConfigError> {
    let home = options.home.clone().or_else(|| {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
    });
    let env = options
        .env
        .clone()
        .unwrap_or_else(|| std::env::vars().collect());
    let workspace = options.workspace.clone().unwrap_or_else(|| ".".into());
    check_files(
        &workspace,
        home.as_deref(),
        options.config_file.as_deref(),
        &env,
    )?;
    let mut cli = options.overrides.clone();
    if let Some(data_dir) = &options.data_dir {
        let data_dir = data_dir.to_string_lossy().into_owned();
        cli.insert(
            0,
            Override::value("storage.data_dir", toml::Value::String(data_dir)),
        );
    }
    let mut config = EffectiveConfig::layer(LoadOptions {
        workspace,
        home,
        custom_file: options.config_file.clone(),
        env,
        cli,
    })
    .map_err(|error| classify(error, ConfigError::Settings))?;
    let directory = std::env::current_dir().map_err(|error| {
        ConfigError::Settings(format!("cannot read the current directory: {error}"))
    })?;
    config.anchor_paths(&directory);
    config
        .settings()
        .map_err(|error| classify(error, ConfigError::Settings))?;
    Ok(config)
}

/// The files [`EffectiveConfig::layer`] reads, checked first so a broken file
/// is reported with its path.
fn check_files(
    workspace: &Path,
    home: Option<&Path>,
    custom: Option<&Path>,
    env: &[(String, String)],
) -> Result<(), ConfigError> {
    let custom = custom.map(Path::to_path_buf).or_else(|| {
        env.iter()
            .find(|(name, _)| name == "BUBBLEPAW_CONFIG")
            .map(|(_, path)| PathBuf::from(path))
    });
    let files = match custom {
        Some(path) => vec![(path, "custom", true)],
        None => home
            .map(|home| (home.join(".bubblepaw/config.toml"), "user", false))
            .into_iter()
            .chain([(workspace.join(".bubblepaw/config.toml"), "project", false)])
            .collect(),
    };
    for (path, layer, required) in files {
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if !required && error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                let message = format!("cannot read {layer} config {}: {error}", path.display());
                return Err(ConfigError::File { path, message });
            }
        };
        // Like the loader, never echo the parser's snippet: it can hold a credential.
        if text.parse::<toml::Table>().is_err() {
            let message = format!("invalid TOML in {layer} config {}", path.display());
            return Err(ConfigError::File { path, message });
        }
    }
    Ok(())
}

/// Where a host keeps its store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoragePaths {
    pub data_dir: PathBuf,
    /// The single-file archive migrated once into `data_dir`.
    pub legacy_db: PathBuf,
    /// Whether `data_dir` is the central default, the only store that imports
    /// its legacy siblings.
    pub central: bool,
}

/// The data directory from `storage.data_dir`, or the central default.
pub fn storage_paths(settings: &Settings) -> Result<StoragePaths, ConfigError> {
    let central = settings.storage.data_dir.as_os_str().is_empty();
    let data_dir = if central {
        crate::legacy_import::default_data_dir()
            .map_err(|error| ConfigError::Storage(format!("{error:#}")))?
    } else {
        settings.storage.data_dir.clone()
    };
    let legacy_db = if settings.storage.legacy_db.as_os_str().is_empty() {
        data_dir.join("archive.sqlite")
    } else {
        settings.storage.legacy_db.clone()
    };
    Ok(StoragePaths {
        data_dir,
        legacy_db,
        central,
    })
}

/// The store retention and cold-storage options from `storage.*`.
pub fn store_options(settings: &Settings) -> StoreOptions {
    let storage = &settings.storage;
    StoreOptions {
        finished_event_retention_hours: storage.finished_event_retention_hours,
        telemetry_retention_days: storage.telemetry_retention_days,
        archive_after_days: storage.archive_after_days,
        cold_tail_messages: storage.cold_tail_messages,
        cold_min_messages: storage.cold_min_messages,
        cold_min_bytes: storage.cold_min_bytes,
        cold_row_min_bytes: storage.cold_row_min_bytes,
    }
}

/// Migrate the legacy archive once and open the store. Another host's
/// ownership is reported as [`crate::host_attach::is_owned`] recognizes it.
pub fn open_store(data_dir: &Path, legacy_db: &Path, settings: &Settings) -> anyhow::Result<Store> {
    Store::migrate_legacy(legacy_db, data_dir)?;
    Store::open_with_options(data_dir, store_options(settings))
}

/// The housekeeping a host runs on its store before serving: spilled
/// outputs, archive retention and inactive child worktrees.
pub fn prepare_store(
    store: &Store,
    data_dir: &Path,
    settings: &Settings,
) -> anyhow::Result<Vec<CleanupReport>> {
    crate::spill::SpillStore::new(data_dir)?.attach(store);
    crate::sessions::prune_archives(store, settings.storage.archived_retention_days)?;
    crate::worktrees::cleanup_inactive(store)
}

/// A checked CLI host configuration: what `bubblepaw stdio` would serve with
/// the same options.
#[derive(Clone)]
pub struct HostConfig {
    request: PreparedRequest,
    settings: Settings,
    central: bool,
}

impl HostConfig {
    /// Layer the configuration like `bubblepaw`, place the store, and check
    /// the provider setup, the main model's route and the model roles.
    pub fn load(options: ConfigOptions) -> Result<Self, ConfigError> {
        let effective = layer(&options)?;
        let settings = effective
            .settings()
            .map_err(|error| classify(error, ConfigError::Settings))?;
        let path = options.workspace.clone().unwrap_or_else(|| ".".into());
        let workspace = path
            .canonicalize()
            .ok()
            .filter(|workspace| workspace.is_dir())
            .ok_or_else(|| ConfigError::Workspace {
                message: format!("workspace {} must be an existing directory", path.display()),
                path,
            })?;
        let paths = storage_paths(&settings)?;
        let absolute = |path: &Path| {
            std::path::absolute(path).map_err(|error| ConfigError::Storage(error.to_string()))
        };
        let data_dir = absolute(&paths.data_dir)?;
        let legacy_db = absolute(&paths.legacy_db)?;
        LiveProviderConfig::new(effective.clone())
            .map_err(|error| classify(error, ConfigError::Providers))?;
        let mut arguments: Vec<OsString> = vec!["bubblepaw".into()];
        arguments.extend(["--workspace".into(), workspace.clone().into()]);
        arguments.extend(["--data-dir".into(), data_dir.clone().into()]);
        if let Some(file) = &options.config_file {
            arguments.extend(["--config".into(), file.clone().into()]);
        }
        arguments.push("stdio".into());
        let cli = Cli::try_parse_from(arguments)
            .map_err(|error| ConfigError::Settings(error.to_string()))?;
        Ok(Self {
            request: PreparedRequest {
                cli,
                effective,
                delegation: None,
                task_prompt: None,
                workspace,
                data_dir,
                legacy_db,
            },
            settings,
            central: paths.central,
        })
    }

    /// The absolute data directory the host owns.
    pub fn data_dir(&self) -> &Path {
        &self.request.data_dir
    }

    /// The canonical workspace.
    pub fn workspace(&self) -> &Path {
        &self.request.workspace
    }

    /// The layered configuration, as `config.get` reports it.
    pub fn effective(&self) -> &EffectiveConfig {
        &self.request.effective
    }

    /// The checked settings.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Whether the data directory is the central default.
    pub fn is_central(&self) -> bool {
        self.central
    }

    /// Migrate the legacy archive once and open the store, as `bubblepaw` does.
    pub fn open_store(&self) -> anyhow::Result<Store> {
        open_store(
            &self.request.data_dir,
            &self.request.legacy_db,
            &self.settings,
        )
    }

    /// The request a host context is started from.
    pub fn into_request(self) -> PreparedRequest {
        self.request
    }
}

#[cfg(test)]
#[path = "host_config_tests.rs"]
mod tests;
