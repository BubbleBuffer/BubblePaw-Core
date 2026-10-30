//! The closed set of settable config key paths.

/// Whether `path` names a settable key; unknown keys are rejected before deserialization.
pub(super) fn known_path(path: &str) -> bool {
    let parts: Vec<_> = path.split('.').collect();
    match parts.as_slice() {
        [
            "model" | "agent" | "storage" | "providers" | "roles" | "context" | "approvals"
            | "agents" | "web" | "extensions" | "mcp" | "exec" | "skills" | "delegate" | "quota",
        ] => true,
        ["delegate", "classes"] => true,
        ["quota", "max_wait_seconds"] => true,
        ["delegate", "classes", name] => !name.is_empty(),
        ["delegate", "classes", name, "default" | "candidates"] => !name.is_empty(),
        [
            "exec",
            "sync_timeout_secs" | "max_jobs_per_session" | "max_jobs" | "max_output_bytes",
        ] => true,
        [
            "storage",
            "data_dir"
            | "finished_event_retention_hours"
            | "telemetry_retention_days"
            | "archive_after_days"
            | "archived_retention_days"
            | "cold_tail_messages"
            | "cold_min_messages"
            | "cold_min_bytes"
            | "cold_row_min_bytes",
        ] => true,
        ["model", "main" | "provider" | "fallbacks"] => true,
        ["agent", "system_prompt" | "max_rounds" | "execution_mode"] => true,
        ["providers", name] => !name.is_empty(),
        [
            "providers",
            name,
            "kind" | "account" | "base_url" | "key_env" | "auth_file" | "reasoning" | "models",
        ] => !name.is_empty(),
        [
            "providers",
            name,
            "reasoning",
            "thinking" | "reasoning_effort",
        ] => !name.is_empty(),
        ["providers", name, "reasoning", "thinking", "type"] => !name.is_empty(),
        ["roles", "summary" | "chapter" | "decider" | "approver"] => true,
        [
            "roles",
            "summary" | "chapter" | "decider" | "approver",
            "models" | "repeats" | "threshold" | "reasoning_effort" | "max_output_tokens",
        ] => true,
        [
            "context",
            "strategy"
            | "memory_files"
            | "compact_threshold_tokens"
            | "window_tokens"
            | "auto_summarize"
            | "retry_base_ms"
            | "classical"
            | "lcm",
        ] => true,
        ["context", "classical", "verbatim_user_tokens"] => true,
        [
            "context",
            "lcm",
            "summary_input"
            | "episode_prompt"
            | "chapter_prompt"
            | "decider_enabled"
            | "source_target_tokens"
            | "source_max_tokens"
            | "summary_target_tokens"
            | "chapter_summary_max_tokens"
            | "chapter_description_tokens"
            | "chapter_rollover_tokens"
            | "decision_context_tokens"
            | "tool_evidence_tokens"
            | "raw_tail_tokens"
            | "index_tokens"
            | "output_reserve_tokens"
            | "apply_target_tokens"
            | "child_compact_threshold_tokens",
        ] => true,
        ["skills", "enabled" | "dirs"] => true,
        [
            "approvals",
            "confirm" | "tools" | "deny" | "read_only" | "hardline" | "auto_prompt",
        ] => true,
        ["approvals", "tools", name] => !name.is_empty(),
        [
            "agents",
            "max_depth"
            | "max_active_per_parent"
            | "max_active_total"
            | "max_retained"
            | "stale_reminder_secs"
            | "auto_wake"
            | "wake_debounce_ms"
            | "worker_retention_days"
            | "worker_keep_all",
        ] => true,
        [
            "web",
            "enabled" | "search" | "fetch_backend" | "key_env" | "provider_key_envs"
            | "deepseek_key_env" | "deepseek_base_url" | "allow_private" | "user_agent"
            | "cache_ttl_secs" | "cache_max_bytes" | "max_searches" | "max_fetches"
            | "cooldown_secs" | "expose",
        ] => true,
        ["web", "expose", "fetch" | "search"] => true,
        ["extensions", name] => !name.is_empty(),
        [
            "extensions",
            name,
            "command" | "account" | "args" | "cwd" | "env",
        ] => !name.is_empty(),
        ["extensions", name, "env", variable] => !name.is_empty() && !variable.is_empty(),
        ["mcp", "servers" | "header_key_envs"] => true,
        ["mcp", "servers", name] => !name.is_empty(),
        [
            "mcp",
            "servers",
            name,
            "command"
            | "url"
            | "headers_from"
            | "args"
            | "cwd"
            | "env"
            | "env_from"
            | "enabled"
            | "defer"
            | "approval"
            | "startup_timeout_secs"
            | "call_timeout_secs"
            | "include_tools"
            | "exclude_tools"
            | "tools",
        ] => !name.is_empty(),
        ["mcp", "servers", name, "env" | "env_from", _] => !name.is_empty(),
        ["mcp", "servers", name, "headers_from", header @ ..] => {
            !name.is_empty() && !header.is_empty() && !header.join(".").is_empty()
        }
        ["mcp", "servers", name, "tools", _] => !name.is_empty(),
        ["mcp", "servers", name, "tools", _, "approval"] => !name.is_empty(),
        _ => false,
    }
}
