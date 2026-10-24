//! Listening TCP sockets owned by a process, read from /proc (Linux only).
use std::collections::HashSet;

/// TCP and TCP6 sockets in LISTEN state owned by `pid`, from /proc.
pub fn tcp_listeners(pid: u32) -> Vec<String> {
    let owned: HashSet<String> = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .unwrap()
        .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
        .filter_map(|link| {
            let link = link.to_string_lossy().into_owned();
            link.strip_prefix("socket:[")?
                .strip_suffix(']')
                .map(str::to_owned)
        })
        .collect();
    let mut listeners = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        for line in std::fs::read_to_string(table)
            .unwrap_or_default()
            .lines()
            .skip(1)
        {
            let fields: Vec<&str> = line.split_whitespace().collect();
            // Field 3 is the state (0A = LISTEN); field 9 is the socket inode.
            if fields.len() > 9 && fields[3] == "0A" && owned.contains(fields[9]) {
                listeners.push(line.to_owned());
            }
        }
    }
    listeners
}
