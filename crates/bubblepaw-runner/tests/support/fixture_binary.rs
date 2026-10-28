use std::path::{Path, PathBuf};

/// Cargo paths support ordinary tests; siblings support copied cross-built tests.
pub fn resolve(cargo_path: &str, name: &str) -> PathBuf {
    let cargo_path = Path::new(cargo_path);
    if cargo_path.is_file() {
        return cargo_path.to_owned();
    }
    let test = std::env::current_exe().expect("current test executable");
    let sibling = test
        .parent()
        .expect("test executable directory")
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        sibling.is_file(),
        "fixture binary missing at Cargo path {} and sibling {}",
        cargo_path.display(),
        sibling.display()
    );
    sibling
}
