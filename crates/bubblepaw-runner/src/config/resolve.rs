//! Resolving the layered config into typed settings.
use super::*;

impl EffectiveConfig {
    pub fn settings(&self) -> Result<Settings> {
        self.settings_for_main(None)
    }
    pub fn settings_for_main(&self, main: Option<&str>) -> Result<Settings> {
        validate_mcp(self.value.get("mcp"))?;
        let mut raw: RawSettings = self
            .value
            .clone()
            .try_into()
            .map_err(|_| anyhow::Error::new(redact::ConfigTypeError))?;
        ensure!(
            !raw.model.main.trim().is_empty(),
            "model.main must not be empty"
        );
        ensure!(
            !raw.model.provider.trim().is_empty(),
            "model.provider must not be empty"
        );
        ensure!(
            raw.agent.max_rounds != Some(0),
            "agent.max_rounds must be positive"
        );
        let qualified = main.map(str::to_owned).unwrap_or_else(|| {
            if raw.model.main.contains('/') {
                raw.model.main.clone()
            } else {
                format!("{}/{}", raw.model.provider, raw.model.main)
            }
        });
        let mut fallback_ids = BTreeSet::new();
        for fallback in &raw.model.fallbacks {
            ensure!(
                fallback
                    .split_once('/')
                    .is_some_and(|(provider, model)| !provider.trim().is_empty()
                        && !model.trim().is_empty()
                        && (raw
                            .providers
                            .get(provider)
                            .is_some_and(|settings| !matches!(
                                settings.kind.as_deref(),
                                Some(
                                    "decisions"
                                        | "openai-decisions"
                                        | "openrouter-decisions"
                                        | "opencode-decisions"
                                )
                            ))
                            || raw.extensions.contains_key(provider))),
                "model.fallbacks must name configured LLM routes as <provider>/<model>"
            );
            ensure!(
                fallback != &qualified && fallback_ids.insert(fallback),
                "model.fallbacks must not repeat the primary or another fallback"
            );
        }
        // Keep the legacy disabled-endpoint switch at the reference boundary,
        // so custom chapter/decision references follow the selected summary too.
        if raw
            .providers
            .get("opencode")
            .and_then(|p| p.base_url.as_deref())
            .is_some_and(|url| url.trim().is_empty())
            && self
                .sources
                .get("roles.summary.models")
                .is_some_and(|source| source == "default")
            && let Some(RoleEntry::Table(summary)) = raw.roles.get_mut("summary")
        {
            summary.models = vec!["main".into()];
        }
        let mut roles =
            ModelRoles::for_models("opencode/deepseek-v4.1-flash".into(), qualified.clone());
        for (name, target) in [
            ("summary", &mut roles.summary),
            ("chapter", &mut roles.chapter),
            ("decider", &mut roles.decider),
            ("approver", &mut roles.approver),
        ] {
            let entry = raw
                .roles
                .get(name)
                .with_context(|| format!("Missing config key: roles.{name}"))?;
            target.models = resolve_role(name, &raw.roles, &qualified, &mut Vec::new())?;
            if let RoleEntry::Table(entry) = entry {
                target.reasoning_effort = entry.reasoning_effort.unwrap_or_default();
                target.max_output_tokens = entry.max_output_tokens;
                if let Some(repeats) = entry.repeats {
                    target.repeats = Some(repeats);
                }
                if let Some(threshold) = entry.threshold {
                    target.threshold = Some(threshold);
                }
            }
        }
        roles.validate().map_err(crate::host_config::role_error)?;
        let strategy = match raw.context.strategy.as_str() {
            "lcm" => ContextStrategy::Lcm,
            "classical" => ContextStrategy::Classical,
            value => {
                bail!("Invalid config value: context.strategy={value:?}; expected lcm or classical")
            }
        };
        ensure!(
            raw.context.compact_threshold_tokens > 0,
            "context.compact_threshold_tokens must be positive"
        );
        ensure!(
            raw.context.window_tokens.is_none_or(|n| n > 0),
            "context.window_tokens must be positive"
        );
        ensure!(
            raw.context.classical.verbatim_user_tokens > 0,
            "context.classical.verbatim_user_tokens must be positive"
        );
        raw.context.lcm.validate()?;
        for (name, provider) in &mut raw.providers {
            provider.validate(name)?;
            if let Some(path) = provider
                .auth_file
                .as_ref()
                .filter(|path| path.is_relative())
                && let Some(base) = self.bases.get(&format!("providers.{name}.auth_file"))
            {
                provider.auth_file = Some(base.join(path));
            }
            if provider.kind.as_deref() == Some("codex") && provider.auth_file.is_none() {
                provider.auth_file = self.default_codex_auth_file.clone();
            }
        }
        for mode in std::iter::once(&raw.approvals.confirm).chain(raw.approvals.tools.values()) {
            ensure!(
                matches!(mode.as_str(), "ask" | "allow" | "deny" | "auto"),
                "Invalid approval mode"
            );
        }
        ensure!(
            raw.approvals
                .deny
                .iter()
                .all(|rule| !rule.trim().is_empty()),
            "approvals.deny entries must be nonempty exact tool names"
        );
        raw.agents.validate()?;
        raw.web.validate()?;
        raw.delegate.validate()?;
        raw.exec.validate()?;
        raw.remote.validate()?;
        crate::toolsets::validate(&raw.toolsets, &raw.agent.toolset)?;
        for directory in &mut raw.skills.dirs {
            if directory.is_relative() {
                *directory = self
                    .bases
                    .get("skills.dirs")
                    .unwrap_or(&self.load_options.workspace)
                    .join(&*directory);
            }
        }
        {
            let path = &mut raw.storage.data_dir;
            if !path.as_os_str().is_empty()
                && path.is_relative()
                && let Some(base) = self.bases.get("storage.data_dir")
            {
                *path = base.join(&*path);
            }
        }
        let mut extensions = Vec::new();
        for (id, extension) in raw.extensions {
            ensure!(
                valid_id(&id) && !extension.command.is_empty(),
                "extension entries need a valid id and a command"
            );
            validate_account(extension.account.as_deref(), &format!("extensions.{id}"))?;
            ensure!(
                extension
                    .env
                    .keys()
                    .all(|name| crate::provider::valid_extension_env_name(name)),
                "extensions.{id}.env names must be nonempty and contain no '=', ',' or NUL"
            );
            let mut cwd = extension.cwd;
            if let Some(path) = cwd.as_ref().filter(|path| path.is_relative())
                && let Some(base) = self.bases.get(&format!("extensions.{id}.cwd"))
            {
                cwd = Some(base.join(path));
            }
            if cwd.is_none() {
                cwd = self.origin_directory.clone();
            }
            extensions.push(ExtensionConfig {
                id,
                account: extension.account,
                command: extension.command,
                args: extension.args,
                cwd,
                env: extension.env,
            });
        }
        let endpoints = endpoints(&raw.providers)?;
        let configured_providers = raw
            .providers
            .keys()
            .filter(|name| {
                let prefix = format!("providers.{name}.");
                self.sources
                    .iter()
                    .any(|(path, source)| path.starts_with(&prefix) && source != "default")
            })
            .cloned()
            .collect();
        Ok(Settings {
            main_model: raw.model.main,
            main_provider: raw.model.provider,
            main_fallbacks: raw.model.fallbacks,
            agent: raw.agent,
            roles,
            endpoints,
            providers: raw.providers,
            context_strategy: strategy,
            memory_files: raw.context.memory_files,
            skills: raw.skills,
            toolsets: raw.toolsets,
            compact_threshold_tokens: raw.context.compact_threshold_tokens,
            context_window_tokens: raw.context.window_tokens,
            auto_summarize: raw.context.auto_summarize,
            retry_base_ms: raw.context.retry_base_ms,
            verbatim_user_tokens: raw.context.classical.verbatim_user_tokens,
            lcm: raw.context.lcm,
            approvals: raw.approvals,
            agents: raw.agents,
            web: raw.web,
            storage: raw.storage,
            exec: raw.exec,
            quota: raw.quota,
            remote: raw.remote,
            extensions,
            mcp: raw.mcp,
            delegate: raw.delegate,
            configured_providers,
        })
    }
}
impl Settings {
    pub fn root_storage_policy(&self) -> bubblepaw_core::RootStoragePolicy {
        bubblepaw_core::RootStoragePolicy {
            store: bubblepaw_core::StoreOptions {
                finished_event_retention_hours: self.storage.finished_event_retention_hours,
                telemetry_retention_days: self.storage.telemetry_retention_days,
                archive_after_days: self.storage.archive_after_days,
                cold_tail_messages: self.storage.cold_tail_messages,
                cold_min_messages: self.storage.cold_min_messages,
                cold_min_bytes: self.storage.cold_min_bytes,
                cold_row_min_bytes: self.storage.cold_row_min_bytes,
            },
            archived_retention_days: self.storage.archived_retention_days,
        }
    }

    /// Configured route names mapped to their quota and usage account identity.
    /// Aliases share state only when explicitly assigned the same account.
    pub fn provider_accounts(&self) -> BTreeMap<String, String> {
        self.providers
            .iter()
            .map(|(name, provider)| {
                (
                    name.clone(),
                    provider.account.as_ref().unwrap_or(name).clone(),
                )
            })
            .chain(self.extensions.iter().map(|extension| {
                (
                    extension.id.clone(),
                    extension.account.as_ref().unwrap_or(&extension.id).clone(),
                )
            }))
            .collect()
    }
    /// Shared startup/apply checks, before any provider credential is read.
    pub fn validate_provider_setup(&self) -> Result<()> {
        for (name, provider) in &self.providers {
            provider.validate(name)?;
            validate_provider_setup(provider, &self.web.provider_key_envs)?;
        }
        Ok(())
    }
    /// Read keys only at transport registration, after configuration is validated.
    pub fn register_providers(&self, registry: &mut ProviderRegistry) -> Result<()> {
        self.register_providers_with_env(registry, &|name| std::env::var(name).ok())
    }
}
