// Matcher checks only: command strings are inspected, never executed.
use crate::ApprovalPolicy;
use serde_json::json;
use std::path::PathBuf;

fn matched(tool: &str, command: &str) -> Option<String> {
    ApprovalPolicy::default().hardline_match(tool, &json!({ "command": command }), &[])
}

/// Matches `command` run with `cwd` inside a session bound to `roots`.
fn in_dir(command: &str, cwd: &str, roots: &[&str]) -> Option<String> {
    let roots: Vec<PathBuf> = roots.iter().map(|root| PathBuf::from(*root)).collect();
    ApprovalPolicy::default().hardline_match(
        "exec",
        &json!({ "command": command, "cwd": cwd }),
        &roots,
    )
}

#[test]
fn fixed_list_matches_each_pattern_through_normalisation() {
    let root = "rm -rf of the filesystem root";
    let home = "rm -rf of the home directory";
    let drive = "rm -rf of a drive root";
    let workspace = "rm -rf of the workspace root";
    let ps_root = "Remove-Item -Recurse -Force of the filesystem root";
    let ps_home = "Remove-Item -Recurse -Force of the home directory";
    let ps_drive = "Remove-Item -Recurse -Force of a drive root";
    let ps_workspace = "Remove-Item -Recurse -Force of the workspace root";
    let cases = [
        ("git reset --hard", "git reset --hard"),
        ("  git   reset  HEAD~1 --hard", "git reset --hard"),
        ("\"git\" -C repo reset --hard", "git reset --hard"),
        (
            "git --no-pager reset --hard origin/main",
            "git reset --hard",
        ),
        ("GIT RESET --HARD", "git reset --hard"),
        ("git push --force origin main", "git push --force"),
        ("git push origin main -f", "git push --force"),
        ("git push -fu origin main", "git push --force"),
        (
            "git push --force-with-lease --force origin main",
            "git push --force",
        ),
        ("git -C repo push -f", "git push --force"),
        ("git clean -fd", "git clean -f"),
        ("git clean -xdf", "git clean -f"),
        ("git clean --force -d", "git clean -f"),
        ("git clean -f -d", "git clean -f"),
        ("rm -rf /", root),
        ("rm -fr /", root),
        ("rm -r -f /", root),
        ("rm -Rf /", root),
        ("rm --recursive --force /", root),
        ("rm -rf -- /", root),
        ("rm -rf /*", root),
        ("rm -rf \"/\"", root),
        ("sudo rm -rf /", root),
        ("/bin/rm -rf /", root),
        ("true && rm -rf /", root),
        ("sh -c 'rm -rf /'", root),
        ("env FOO=1 rm -rf /", root),
        ("cmd /c rm -rf /", root),
        ("rm -rf ~", home),
        ("rm -rf ~/", home),
        ("rm -rf ~/*", home),
        ("rm -rf $HOME", home),
        ("rm -rf ${HOME}", home),
        ("rm -rf \"$HOME\"", home),
        ("rm -rf C:\\", drive),
        ("rm -rf c:/", drive),
        ("rm -rf .", workspace),
        ("rm -rf ./", workspace),
        ("rm -rf .\\", workspace),
        ("Remove-Item -Recurse -Force C:\\", ps_drive),
        ("Remove-Item -Path C:\\ -Recurse -Force", ps_drive),
        ("remove-item -force -recurse ~", ps_home),
        ("ri -r -fo .", ps_workspace),
        ("Remove-Item -Recurse -Force /", ps_root),
        (
            "powershell -Command \"Remove-Item -Recurse -Force $HOME\"",
            ps_home,
        ),
        ("mkfs.ext4 /dev/sdb1", "mkfs"),
        ("echo ok; mkfs /dev/sda", "mkfs"),
        ("/sbin/mkfs.xfs /dev/x", "mkfs"),
        ("dd if=/dev/zero of=/dev/sda bs=1M", "dd to /dev/*"),
        ("shutdown -h now", "shutdown"),
        ("sudo reboot", "reboot"),
        ("shutdown.exe /s /t 0", "shutdown"),
        ("timeout 5 reboot", "reboot"),
        ("format C:", "format of a drive letter"),
        ("format d: /q", "format of a drive letter"),
        ("format /fs:ntfs E:", "format of a drive letter"),
        ("cmd /c format c:", "format of a drive letter"),
    ];
    for (command, pattern) in cases {
        assert_eq!(
            matched("exec", command).as_deref(),
            Some(pattern),
            "{command}"
        );
    }
}

#[test]
fn near_misses_and_quoted_text_stay_allowed() {
    for command in [
        // Force-with-lease is the allowed force push.
        "git push --force-with-lease origin main",
        "git push --force-with-lease=main:abc origin main",
        "git push origin main",
        "git reset --soft HEAD~1",
        "git clean -n",
        "git status --short",
        "git commit -m \"git reset --hard\"",
        "echo \"rm -rf /\"",
        "echo reboot later",
        "rm -r build",
        "rm -f file.txt",
        "rm -rf ./target",
        "rm -rf build/",
        "rm -rf /tmp/build",
        "rm -rf /home/user/project",
        "rm -rf ~/projects",
        "rm -rf c:\\users\\dev\\out",
        // Contents of the workspace are not the workspace root itself.
        "rm -rf ./*",
        "rm -rf *",
        "Remove-Item -Recurse -Force .\\target",
        "Remove-Item -Force notes.txt",
        "dd if=disk.img of=copy.img",
        "mkdir formatted",
        "cargo fmt --all",
        "format_notes.txt",
        "shutdown-notes.md",
        // Wrapped words are only commands when they are the command word.
        "sudo -u root ls",
    ] {
        assert_eq!(matched("exec", command), None, "{command}");
    }
}

#[test]
fn command_templates_match_their_joined_argv() {
    let policy = ApprovalPolicy::default();
    let template = json!({"command": {"parts": ["rm ", " /"], "values": ["-rf"]}});
    assert_eq!(
        policy.hardline_match("exec", &template, &[]).as_deref(),
        Some("rm -rf of the filesystem root")
    );
    let safe = json!({"command": {"parts": ["git status ", ""], "values": ["--short"]}});
    assert_eq!(policy.hardline_match("exec", &safe, &[]), None);
}

#[test]
fn only_exec_commands_and_only_while_the_floor_is_on() {
    let call = json!({"command": "rm -rf /"});
    let policy = ApprovalPolicy::default();
    assert!(policy.hardline);
    assert_eq!(policy.hardline_match("write", &call, &[]), None);
    assert_eq!(
        policy.hardline_match("exec", &json!({"path": "x"}), &[]),
        None
    );
    let off = ApprovalPolicy {
        hardline: false,
        ..Default::default()
    };
    assert_eq!(off.hardline_match("exec", &call, &[]), None);
}

const PROJECT: &str = r"C:\work\proj";
const PROJECT_ROOT: &str = "rm -rf of the workspace root";

#[test]
fn workspace_root_matches_in_absolute_quoted_and_verbatim_forms() {
    let ps_root = "Remove-Item -Recurse -Force of the workspace root";
    for (command, pattern) in [
        (r#"rm -rf "C:\work\proj""#, PROJECT_ROOT),
        (r"rm -rf C:\work\proj", PROJECT_ROOT),
        (r"rm -rf c:/work/proj/", PROJECT_ROOT),
        (r#"rm -rf "\\?\C:\work\proj""#, PROJECT_ROOT),
        (r#"rm -rf "C:\work\proj\""#, PROJECT_ROOT),
        (r"rm -rf C:\work\proj\child\..", PROJECT_ROOT),
        (r"rm -rf C:\work\proj\..\proj", PROJECT_ROOT),
        (r#"Remove-Item -Recurse -Force "C:\work\proj""#, ps_root),
    ] {
        assert_eq!(
            in_dir(command, ".", &[PROJECT]).as_deref(),
            Some(pattern),
            "{command}"
        );
    }
}

#[test]
fn climbing_back_to_the_root_from_a_subdirectory_is_blocked() {
    for command in [
        "rm -rf ..",
        "rm -rf ../",
        r"rm -rf ..\",
        "rm -rf ./../.",
        "rm -rf ../child/..",
        "Remove-Item -Recurse -Force ..",
    ] {
        let expected = if command.starts_with("Remove") {
            "Remove-Item -Recurse -Force of the workspace root"
        } else {
            PROJECT_ROOT
        };
        assert_eq!(
            in_dir(command, "child", &[PROJECT]).as_deref(),
            Some(expected),
            "{command}"
        );
    }
}

#[test]
fn subdirectory_deletes_stay_allowed_and_contents_are_not_the_root() {
    for (command, cwd) in [
        ("rm -rf .", "child"),
        ("rm -rf ./build", "child"),
        (r"rm -rf ..\sibling", "child"),
        (r#"rm -rf "C:\work\proj\child""#, "."),
        (r#"rm -rf "C:\work""#, "."),
        (r#"rm -rf "C:\work\proj\*""#, "."),
        ("rm -rf ./*", "."),
        ("rm -rf *", "."),
    ] {
        assert_eq!(
            in_dir(command, cwd, &[PROJECT]),
            None,
            "{command}, cwd={cwd}"
        );
    }
}

#[test]
fn cd_moves_the_directory_for_later_segments() {
    assert_eq!(
        in_dir("cd child && rm -rf ..", ".", &[PROJECT]).as_deref(),
        Some(PROJECT_ROOT)
    );
    assert_eq!(in_dir("cd child; rm -rf .", ".", &[PROJECT]), None);
    assert_eq!(
        in_dir("cd .. && rm -rf proj", ".", &[PROJECT]).as_deref(),
        Some(PROJECT_ROOT)
    );
    assert_eq!(
        in_dir("cd child && cd .. && rm -rf .", ".", &[PROJECT]).as_deref(),
        Some(PROJECT_ROOT)
    );
    assert_eq!(
        in_dir("cd \"sub dir\" && rm -rf ..", ".", &[PROJECT]).as_deref(),
        Some(PROJECT_ROOT)
    );
    // A directory the matcher cannot follow is taken as the root for `.`.
    assert_eq!(
        in_dir("cd $dir && rm -rf .", ".", &[PROJECT]).as_deref(),
        Some(PROJECT_ROOT)
    );
}

#[test]
fn quoted_workspace_paths_with_spaces_are_one_path() {
    let root = r"C:\my work\proj";
    assert_eq!(
        in_dir(r#"rm -rf "C:\my work\proj""#, ".", &[root]).as_deref(),
        Some(PROJECT_ROOT)
    );
    assert_eq!(
        in_dir(r#"rm -rf "C:\my work\proj\child""#, ".", &[root]),
        None
    );
}

#[test]
fn climbing_past_a_posix_workspace_reaches_the_filesystem_root() {
    assert_eq!(
        in_dir("rm -rf ../../../..", "child", &["/work/proj"]).as_deref(),
        Some("rm -rf of the filesystem root")
    );
    assert_eq!(in_dir("rm -rf ../../child", "child", &["/work/proj"]), None);
}

#[test]
fn without_a_bound_workspace_only_the_literal_forms_match() {
    assert_eq!(in_dir("rm -rf .", ".", &[]).as_deref(), Some(PROJECT_ROOT));
    assert_eq!(in_dir(r"rm -rf C:\work\proj", ".", &[]), None);
    assert_eq!(in_dir("rm -rf ..", "child", &[]), None);
}

#[test]
fn every_protected_root_counts_and_the_first_one_sets_the_directory() {
    let roots = [PROJECT, r"D:\backup"];
    assert_eq!(
        in_dir(r"rm -rf D:\backup", "child", &roots).as_deref(),
        Some(PROJECT_ROOT)
    );
    assert_eq!(
        in_dir("rm -rf ..", "child", &roots).as_deref(),
        Some(PROJECT_ROOT)
    );
}

#[test]
fn hardline_is_inherited_from_the_host_and_cannot_be_turned_off() {
    let host_on = ApprovalPolicy::default();
    let host_off = ApprovalPolicy {
        hardline: false,
        ..Default::default()
    };
    // A run override or child profile takes the host's setting either way.
    assert!(
        !ApprovalPolicy::default()
            .with_inherited_hardline(&host_off)
            .hardline
    );
    assert!(
        !ApprovalPolicy {
            hardline: false,
            ..Default::default()
        }
        .with_inherited_hardline(&host_off)
        .hardline
    );
    assert!(
        ApprovalPolicy::default()
            .with_inherited_hardline(&host_on)
            .hardline
    );
    assert!(
        ApprovalPolicy {
            hardline: false,
            ..Default::default()
        }
        .with_inherited_hardline(&host_on)
        .hardline
    );
}
