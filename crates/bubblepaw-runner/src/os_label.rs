//! The operating system in the `Execution environment` prompt line, with its
//! version, such as `Windows 11 (10.0.26100, x86_64)`. It is read once per
//! process, so the prompt prefix stays byte-stable for caching.
use crate::process_output;
use std::{fs, sync::OnceLock, time::Duration};
use tokio::process::Command;

/// The label for this machine. Any source that cannot be read falls back to
/// `std::env::consts::OS` and the architecture.
pub fn os_label() -> &'static str {
    static LABEL: OnceLock<String> = OnceLock::new();
    LABEL.get_or_init(detect).as_str()
}

fn detect() -> String {
    let os = std::env::consts::OS;
    let found = match os {
        "linux" => linux(),
        "macos" => macos(),
        "windows" => windows(),
        _ => None,
    };
    compose(os, found, std::env::consts::ARCH)
}

/// Joins a found name and its optional detail with the architecture. Without
/// a name the label is the plain OS constant and the architecture.
fn compose(os: &str, found: Option<(String, Option<String>)>, arch: &str) -> String {
    match found {
        Some((name, Some(detail))) => format!("{name} ({detail}, {arch})"),
        Some((name, None)) => format!("{name} ({arch})"),
        None => format!("{os} ({arch})"),
    }
}

fn linux() -> Option<(String, Option<String>)> {
    let release = fs::read_to_string("/etc/os-release")
        .or_else(|_| fs::read_to_string("/usr/lib/os-release"))
        .ok()?;
    let name = parse_os_release(&release)?;
    let kernel = fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .map(|kernel| kernel.trim().to_owned())
        .filter(|kernel| !kernel.is_empty())
        .map(|kernel| format!("Linux {kernel}"));
    Some((name, kernel))
}

fn macos() -> Option<(String, Option<String>)> {
    let plist = fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist").ok()?;
    Some((format!("macOS {}", parse_product_version(&plist)?), None))
}

fn windows() -> Option<(String, Option<String>)> {
    let mut command = Command::new("cmd");
    command.args(["/d", "/c", "ver"]);
    let output = process_output::output_blocking(command, Duration::from_secs(2)).ok()??;
    let (major, minor, build) = parse_ver(&String::from_utf8_lossy(&output.stdout))?;
    Some((
        windows_name(major, minor, build),
        Some(format!("{major}.{minor}.{build}")),
    ))
}

/// The `PRETTY_NAME` of an os-release file, such as `Ubuntu 24.04.1 LTS`.
fn parse_os_release(_text: &str) -> Option<String> {
    None
}

/// The `ProductVersion` of a `SystemVersion.plist`: the first `<string>` after
/// its key, found by plain text search.
fn parse_product_version(_plist: &str) -> Option<String> {
    None
}

/// The `major.minor.build` in `cmd /c ver` output, such as
/// `Microsoft Windows [Version 10.0.26100.2033]`.
fn parse_ver(_output: &str) -> Option<(u32, u32, u32)> {
    None
}

/// The product name for a Windows version. Build 22000 and later is Windows 11;
/// other major versions print as `major.minor`.
fn windows_name(_major: u32, _minor: u32, _build: u32) -> String {
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_name_the_system_and_garbage_falls_back() {
        const UBUNTU: &str = "NAME=\"Ubuntu\"\nVERSION=\"24.04.1 LTS (Noble Numbat)\"\nPRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\n";
        const PLIST: &str = "<dict>\n\t<key>ProductName</key>\n\t<string>macOS</string>\n\t<key>ProductVersion</key>\n\t<string>15.1</string>\n</dict>";
        assert_eq!(
            parse_os_release(UBUNTU).as_deref(),
            Some("Ubuntu 24.04.1 LTS")
        );
        assert_eq!(parse_product_version(PLIST).as_deref(), Some("15.1"));
        for (output, name) in [
            (
                "\r\nMicrosoft Windows [Version 10.0.26100.2033]\r\n",
                "Windows 11",
            ),
            ("Microsoft Windows [Version 10.0.19045.5011]", "Windows 10"),
        ] {
            let (major, minor, build) = parse_ver(output).expect("ver output parses");
            assert_eq!(windows_name(major, minor, build), name);
        }
        assert_eq!(parse_os_release("not an os-release file"), None);
        assert_eq!(parse_product_version("<dict></dict>"), None);
        assert_eq!(parse_ver("garbage"), None);
        assert_eq!(compose("linux", None, "x86_64"), "linux (x86_64)");
        assert_eq!(
            compose(
                "linux",
                Some((
                    "Ubuntu 24.04.1 LTS".to_owned(),
                    Some("Linux 6.8.0-45-generic".to_owned())
                )),
                "x86_64"
            ),
            "Ubuntu 24.04.1 LTS (Linux 6.8.0-45-generic, x86_64)"
        );
    }
}
