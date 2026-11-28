//! Hardline exec floor: other spellings of protected targets. Matcher checks
//! only; command strings are inspected, never executed.
use bubblepaw_core::ApprovalPolicy;
use serde_json::json;
use std::path::{Path, PathBuf};

fn matched(command: &str) -> Option<String> {
    in_dir(command, ".", &[])
}

/// Matches `command` run with `cwd` inside a session bound to `roots`.
fn in_dir(command: &str, cwd: &str, roots: &[&str]) -> Option<String> {
    let roots: Vec<PathBuf> = roots.iter().map(PathBuf::from).collect();
    ApprovalPolicy::default().hardline_match(
        "exec",
        &json!({ "command": command, "cwd": cwd }),
        &roots,
    )
}

/// The host's home directory in every spelling a shell accepts for it.
fn home_spellings() -> Vec<String> {
    let home = std::env::home_dir().expect("the test host has a home directory");
    let mut spellings = spellings_of(&home);
    spellings.extend(home.parent().into_iter().flat_map(spellings_of));
    spellings
}

/// One path in the spellings a shell accepts for it.
fn spellings_of(home: &Path) -> Vec<String> {
    let native = home.to_string_lossy().into_owned();
    let slashes = native.replace('\\', "/");
    let mut spellings = vec![
        native.clone(),
        format!("\"{native}\""),
        slashes.clone(),
        format!("{native}/"),
        format!("{native}/."),
        format!("{native}/./"),
        format!("{native}//"),
        format!("{native}/x/.."),
        native.to_uppercase(),
    ];
    if let [letter, b':', rest @ ..] = slashes.as_bytes() {
        let (letter, rest) = (*letter as char, String::from_utf8_lossy(rest));
        spellings.push(format!("/{}{rest}", letter.to_ascii_lowercase()));
        spellings.push(format!("/{}{rest}", letter.to_ascii_uppercase()));
        spellings.push(format!("/cygdrive/{}{rest}", letter.to_ascii_lowercase()));
        spellings.push(format!("{}:{rest}", letter.to_ascii_lowercase()));
    }
    spellings
}

#[test]
fn spelling_variants_of_protected_targets_are_refused() {
    let mut refused: Vec<String> = [
        // Command word: escapes and quoting, in the sh reading.
        r"r\m -rf /",
        r"\rm -rf /",
        r#""rm" -rf /"#,
        "'rm' -rf /",
        r#"r""m -rf /"#,
        "r''m -rf /",
        "r^m -rf /",
        r"sudo r\m -rf ~",
        // Places: dot segments, repeated and trailing separators, `..`.
        "rm -rf ~/.",
        "rm -rf ~/./",
        "rm -rf ~//",
        "rm -rf $HOME/.",
        "rm -rf ${HOME}/./",
        "rm -rf ~/x/..",
        // Every ancestor of the home directory.
        "rm -rf ~/..",
        "rm -rf $HOME/../..",
        "rm -rf /.",
        "rm -rf /./",
        "rm -rf /tmp/..",
        r"rm -rf C:\.",
        r"rm -rf %USERPROFILE%\.",
        "Remove-Item -Recurse -Force $env:USERPROFILE/.",
        // The working directory by variable or substitution.
        "rm -rf $PWD",
        "rm -rf ${PWD}",
        "rm -rf \"$PWD\"",
        "rm -rf $(pwd)",
        "rm -rf `pwd`",
        "rm -rf %CD%",
        "Remove-Item -Recurse -Force $PWD",
    ]
    .map(String::from)
    .to_vec();
    for home in home_spellings() {
        refused.push(format!("rm -rf {home}"));
        refused.push(format!("Remove-Item -Recurse -Force {home}"));
    }
    for command in &refused {
        assert!(matched(command).is_some(), "{command}");
    }
    // Ordinary commands stay allowed, and the variable forms mean `.`.
    for command in [
        "rm -rf ./build",
        "rm -rf ~/project/target",
        "rm -rf \"$PWD/dist\"",
        "rm -rf $(pwd)/dist",
        "rm -rf \"$HOME/my project/target\"",
    ] {
        assert_eq!(matched(command), None, "{command}");
    }
    let project = r"C:\work\proj";
    assert_eq!(in_dir("rm -rf \"$PWD\"", "sub", &[project]), None);
    assert_eq!(
        in_dir("rm -rf \"$PWD\"", ".", &[project]).as_deref(),
        Some("rm -rf of the workspace root")
    );
}
