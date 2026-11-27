//! Paths read lexically for the hardline floor (see `blocklist.rs`).

use std::sync::OnceLock;

/// Spellings of the home directory that start a word. Read case-folded.
const HOME_WORDS: &[&str] = &[
    "~",
    "$home",
    "$userprofile",
    "%userprofile%",
    "$env:userprofile",
    "$env:home",
];

/// A path read lexically: its drive, whether it is rooted and its normal parts.
/// Nothing touches the filesystem, so a missing path still has a place.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Place {
    drive: String,
    absolute: bool,
    parts: Vec<String>,
}

impl Place {
    /// Lowercased, with `/` separators, a Windows verbatim prefix (`\\?\`) removed,
    /// and `.` and `..` folded. On Windows the shell may be Git Bash, whose
    /// drive paths (`/c/users`, `/cygdrive/c/users`) name `c:/users`.
    pub(super) fn new(path: &str) -> Self {
        Self::parse(path, cfg!(windows))
    }

    pub(super) fn parse(path: &str, msys: bool) -> Self {
        let folded = path.to_lowercase().replace('\\', "/");
        let folded = folded.strip_prefix("//?/").unwrap_or(&folded);
        let msys_path;
        let folded = if msys {
            msys_path = msys_drive(folded);
            msys_path.as_str()
        } else {
            folded
        };
        let (drive, rest) = match folded.as_bytes() {
            [letter, b':', ..] if letter.is_ascii_alphabetic() => folded.split_at(2),
            _ => ("", folded),
        };
        let absolute = !drive.is_empty() || rest.starts_with('/');
        let mut parts = Vec::new();
        push_parts(&mut parts, absolute, rest.split('/'));
        Self {
            drive: drive.to_owned(),
            absolute,
            parts,
        }
    }

    /// A target that starts with a home spelling (`~`, `$HOME/x`, `%USERPROFILE%\x`)
    /// as a place under the host's home directory. None for other targets.
    pub(super) fn under_home(target: &str) -> Option<Self> {
        let lowered = target.to_lowercase();
        let end = lowered.find(['/', '\\']).unwrap_or(lowered.len());
        let (head, rest) = lowered.split_at(end);
        HOME_WORDS
            .contains(&head)
            .then(|| host_home().join(&Self::parse(rest, false)))
    }

    pub(super) fn join(&self, relative: &Place) -> Place {
        let mut parts = self.parts.clone();
        push_parts(
            &mut parts,
            self.absolute,
            relative.parts.iter().map(String::as_str),
        );
        Place {
            drive: self.drive.clone(),
            absolute: self.absolute,
            parts,
        }
    }

    pub(super) fn is_absolute(&self) -> bool {
        self.absolute
    }

    pub(super) fn is_root(&self) -> bool {
        self.absolute && self.drive.is_empty() && self.parts.is_empty()
    }

    pub(super) fn is_drive_root(&self) -> bool {
        self.absolute && !self.drive.is_empty() && self.parts.is_empty()
    }

    pub(super) fn is_home(&self) -> bool {
        self == host_home()
    }

    /// No parts: for a relative place, the directory itself.
    pub(super) fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }
}

/// The host's home directory, resolved once from the host process. When the
/// host reports none, a stand-in place that only home spellings can reach.
fn host_home() -> &'static Place {
    static HOME: OnceLock<Place> = OnceLock::new();
    HOME.get_or_init(|| {
        std::env::home_dir()
            .filter(|path| !path.as_os_str().is_empty())
            .map(|path| Place::new(&path.to_string_lossy()))
            .filter(|place| !place.is_root() && !place.is_drive_root() && place.is_absolute())
            .unwrap_or_else(|| Place {
                drive: "~".into(),
                absolute: true,
                parts: Vec::new(),
            })
    })
}

/// Appends path segments, folding `.` and `..`. A rooted path cannot climb above its root.
fn push_parts<'a>(
    parts: &mut Vec<String>,
    absolute: bool,
    segments: impl Iterator<Item = &'a str>,
) {
    for segment in segments {
        match segment {
            "" | "." => {}
            ".." => {
                let climb = parts.last().is_some_and(|last| last.as_str() != "..");
                if climb {
                    parts.pop();
                } else if !absolute {
                    parts.push(segment.to_owned());
                }
            }
            other => parts.push(other.to_owned()),
        }
    }
}

/// Git Bash's spelling of a drive path: `/c/users` and `/cygdrive/c/users` are
/// `c:/users`. Other paths are returned unchanged.
fn msys_drive(path: &str) -> String {
    let path = match path.strip_prefix("/cygdrive/") {
        Some(rest) => format!("/{rest}"),
        None => path.to_owned(),
    };
    match path.as_bytes() {
        [b'/', letter, rest @ ..]
            if letter.is_ascii_alphabetic() && matches!(rest.first(), None | Some(b'/')) =>
        {
            format!("{}:{}", *letter as char, &path[2..])
        }
        _ => path,
    }
}

/// A drive letter with an optional separator, such as `C:` or `d:\`.
pub(super) fn is_drive(text: &str) -> bool {
    let text = text.trim_end_matches(['/', '\\']);
    let bytes = text.as_bytes();
    bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}
