//! Paths as the model should see them. Windows canonicalization yields the
//! verbatim form (`\\?\C:\...`), which cmd.exe cannot use as a directory and
//! which the model copies back into tools. Prompts and notes print this form.
use std::path::Path;

/// `\\?\C:\x` becomes `C:\x` and `\\?\UNC\host\share` becomes `\\host\share`;
/// every other path is returned as its display text.
pub fn plain_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbatim_prefixes_are_dropped_and_other_paths_kept() {
        for (given, plain) in [
            (r"\\?\C:\Users\dev\ws", r"C:\Users\dev\ws"),
            (r"\\?\UNC\host\share\ws", r"\\host\share\ws"),
            (r"C:\Users\dev\ws", r"C:\Users\dev\ws"),
            ("/home/dev/ws", "/home/dev/ws"),
        ] {
            assert_eq!(plain_path(Path::new(given)), plain);
        }
    }
}
