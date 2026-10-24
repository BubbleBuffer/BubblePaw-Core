//! Experimental command templates deliberately support one native invocation,
//! not a general shell grammar. Strings continue to be ordinary shell scripts.
use anyhow::{Result, anyhow, ensure};
use serde_json::{Value, json};

const MAX_BYTES: usize = 128 * 1024;
const MAX_ARGS: usize = 1024;

#[derive(Debug, Clone)]
pub enum CommandExecution {
    Shell {
        script: String,
    },
    NativeArgv {
        executable: String,
        args: Vec<String>,
    },
}

#[derive(Debug, Clone)]
pub struct PreparedCommand {
    pub original: Value,
    /// A shell-specific display representation, never executed for templates.
    pub rendered: String,
    pub execution: CommandExecution,
}

impl PreparedCommand {
    pub fn metadata(&self) -> Value {
        match &self.execution {
            CommandExecution::Shell { .. } => json!({
                "execution_mode":"shell", "shell":shell_name(), "experimental":false
            }),
            CommandExecution::NativeArgv { args, .. } => json!({
                "execution_mode":"native_argv", "shell":null, "experimental":true,
                "argument_count":args.len()
            }),
        }
    }
}

pub fn shell_name() -> &'static str {
    if cfg!(windows) { "powershell" } else { "sh" }
}

/// Validate before starting any effects. In v0 every interpolation is a whole
/// unquoted argument; the executable must be literal. Static text supports
/// only plain words separated by spaces/tabs. Shell grammar belongs in strings.
pub fn prepare(command: &Value) -> Result<PreparedCommand> {
    if let Some(script) = command.as_str() {
        ensure!(
            !script.is_empty() && script.len() <= MAX_BYTES,
            "command must contain 1 to 131072 bytes"
        );
        ensure!(!script.contains('\0'), "command cannot contain NUL");
        return Ok(PreparedCommand {
            original: command.clone(),
            rendered: script.to_owned(),
            execution: CommandExecution::Shell {
                script: script.to_owned(),
            },
        });
    }
    let object = command
        .as_object()
        .ok_or_else(|| anyhow!("command must be a string or an experimental cmd template"))?;
    ensure!(
        object.len() == 2 && object.contains_key("parts") && object.contains_key("values"),
        "cmd template requires only parts and values"
    );
    let parts = strings(&command["parts"], "parts")?;
    let values = strings(&command["values"], "values")?;
    ensure!(
        parts.len() == values.len() + 1,
        "cmd parts must have one more entry than values"
    );
    ensure!(values.len() < MAX_ARGS, "cmd exceeds 1024 arguments");
    ensure!(
        parts.iter().chain(&values).map(|v| v.len()).sum::<usize>() <= MAX_BYTES,
        "cmd exceeds 131072 bytes"
    );
    ensure!(
        !parts.iter().chain(&values).any(|v| v.contains('\0')),
        "cmd cannot contain NUL"
    );

    let mut argv = Vec::<String>::new();
    for (index, part) in parts.iter().enumerate() {
        // No shell lexer is involved: an intentionally narrow grammar avoids
        // mistaking quoted words, comments, heredocs or script source for argv.
        ensure!(
            part.chars().all(|c| matches!(c, ' ' | '\t')
                || c.is_alphanumeric()
                || "_./\\:@%+=,-".contains(c)),
            "experimental cmd accepts one native command with plain literal words; use a raw string for shell quotes, expansions, comments, scripts, pipes or redirection"
        );
        if index > 0 {
            ensure!(
                part.is_empty() && index == values.len() || part.starts_with([' ', '\t']),
                "cmd substitutions must be complete unquoted arguments, not embedded words"
            );
        }
        argv.extend(
            part.split([' ', '\t'])
                .filter(|v| !v.is_empty())
                .map(str::to_owned),
        );
        if index < values.len() {
            ensure!(
                part.ends_with([' ', '\t']) && !argv.is_empty(),
                "cmd substitutions must be complete unquoted arguments after a literal executable"
            );
            argv.push(values[index].to_owned());
        }
    }
    ensure!(
        !argv.is_empty() && argv.len() <= MAX_ARGS,
        "cmd must contain a literal executable and at most 1024 arguments"
    );
    let executable = argv.remove(0);
    #[cfg(windows)]
    reject_windows_non_native(&executable)?;
    let mut rendered = std::iter::once(&executable)
        .chain(argv.iter())
        .map(|v| quote_display(v))
        .collect::<Vec<_>>()
        .join(" ");
    if cfg!(windows) {
        rendered.insert_str(0, "& ");
    }
    Ok(PreparedCommand {
        original: command.clone(),
        rendered,
        execution: CommandExecution::NativeArgv {
            executable,
            args: argv,
        },
    })
}

fn strings<'a>(value: &'a Value, name: &str) -> Result<Vec<&'a str>> {
    value
        .as_array()
        .ok_or_else(|| anyhow!("cmd {name} must be a string array"))?
        .iter()
        .map(|v| {
            v.as_str()
                .ok_or_else(|| anyhow!("cmd {name} entries must be strings"))
        })
        .collect()
}

#[cfg(windows)]
fn program_name(executable: &str) -> String {
    executable
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(executable)
        .to_ascii_lowercase()
        .trim_end_matches(".exe")
        .to_owned()
}

#[cfg(windows)]
fn reject_windows_non_native(executable: &str) -> Result<()> {
    let lower = executable.to_ascii_lowercase();
    let extension_form = lower.trim_end_matches('.');
    ensure!(
        !extension_form.ends_with(".cmd") && !extension_form.ends_with(".bat"),
        "experimental cmd requires a native executable; use a raw string for batch scripts"
    );
    let name = program_name(executable);
    if [
        "echo",
        "cd",
        "dir",
        "cls",
        "set",
        "copy",
        "del",
        "type",
        "write-output",
        "write-host",
        "get-childitem",
        "get-content",
        "set-location",
        "ls",
        "cat",
        "pwd",
    ]
    .contains(&name.as_str())
        && !lower.ends_with(".exe")
    {
        anyhow::bail!(
            "experimental cmd requires a native executable; use a raw string for shell builtins or aliases"
        );
    }
    Ok(())
}

fn quote_display(value: &str) -> String {
    if cfg!(windows) {
        format!("'{}'", value.replace('\'', "''"))
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}
