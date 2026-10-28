use anyhow::Result;
use bubblepaw_runner::scratchpads::{ScratchpadStore, WORKSPACE_SCRATCH_MAX_AGE};
use std::{
    fs::{self, File},
    io::Write,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant, SystemTime},
};

fn set_modified(path: &Path, time: SystemTime) -> Result<()> {
    #[cfg(unix)]
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        let duration = time.duration_since(SystemTime::UNIX_EPOCH)?;
        let stamp = libc::timespec {
            tv_sec: duration.as_secs().try_into()?,
            tv_nsec: duration.subsec_nanos().into(),
        };
        let stamps = [stamp, stamp];
        // SAFETY: the CString and two timespec values outlive the syscall. The
        // flag updates the link itself and cannot alter its outside target.
        let result = unsafe {
            libc::utimensat(
                libc::AT_FDCWD,
                path.as_ptr(),
                stamps.as_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        return Ok(());
    }
    let mut options = File::options();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Backup semantics + OPEN_REPARSE_POINT: timestamp junction itself.
        options
            .custom_flags(0x02000000 | 0x00200000)
            .access_mode(0x100);
    }
    options
        .open(path)?
        .set_times(fs::FileTimes::new().set_modified(time))?;
    Ok(())
}

fn age_tree(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    #[cfg(windows)]
    let is_link = {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    };
    #[cfg(not(windows))]
    let is_link = metadata.file_type().is_symlink();
    if metadata.is_dir() && !is_link {
        for entry in fs::read_dir(path)? {
            age_tree(&entry?.path())?;
        }
    }
    set_modified(
        path,
        SystemTime::now() - WORKSPACE_SCRATCH_MAX_AGE - Duration::from_secs(60),
    )
}

#[test]
fn retention_requires_old_nested_contents_and_no_live_host_lease() -> Result<()> {
    let data = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let first = ScratchpadStore::new(data.path())?;
    let lease = first.lease_workspace(workspace.path())?;
    fs::create_dir(lease.path.join("nested"))?;
    fs::write(lease.path.join("nested/file"), "keep")?;
    age_tree(&lease.path)?;
    let second = ScratchpadStore::new(data.path())?;
    assert_eq!(second.sweep_workspace()?, 0, "live host retains old files");
    drop(first);
    assert_eq!(second.sweep_workspace()?, 0, "a cloned lease remains live");
    let path = lease.path.clone();
    drop(lease);
    set_modified(&path.join("nested/file"), SystemTime::now())?;
    assert_eq!(
        second.sweep_workspace()?,
        0,
        "recent nested content prevents deletion"
    );
    age_tree(&path)?;
    assert_eq!(second.sweep_workspace()?, 1);
    assert!(!path.exists());
    assert_eq!(second.sweep_workspace()?, 0);
    Ok(())
}

#[test]
fn reacquiring_an_old_workspace_refreshes_last_use_before_sweep() -> Result<()> {
    let data = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let path = {
        let service = ScratchpadStore::new(data.path())?;
        service.lease_workspace(workspace.path())?.path
    };
    fs::write(path.join("script.txt"), "old script")?;
    age_tree(&path)?;
    {
        let active = ScratchpadStore::new(data.path())?;
        active.lease_workspace(workspace.path())?;
        assert!(
            SystemTime::now().duration_since(fs::metadata(path.join(".last-used"))?.modified()?)?
                < WORKSPACE_SCRATCH_MAX_AGE,
            "reacquiring an empty marker must explicitly refresh its timestamp"
        );
        assert_eq!(active.sweep_workspace()?, 0);
    }
    let sweeper = ScratchpadStore::new(data.path())?;
    assert_eq!(sweeper.sweep_workspace()?, 0, "last use survives host exit");
    assert_eq!(fs::read_to_string(path.join("script.txt"))?, "old script");
    Ok(())
}

#[test]
fn cleanup_registration_does_not_prolong_a_workspace_host_lease() -> Result<()> {
    let data = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let store = bubblepaw_core::Store::in_memory()?;
    let session = store.create_session("cleanup lease")?;
    let path = {
        let service = ScratchpadStore::new(data.path())?;
        service
            .for_session(&store, &session.id, workspace.path())?
            .workspace
    };
    age_tree(&path)?;
    assert_eq!(ScratchpadStore::new(data.path())?.sweep_workspace()?, 1);
    assert!(
        store.session(&session.id).is_ok(),
        "the Store remains alive while the host lease is gone"
    );
    Ok(())
}

fn old_scratch_with_outside_link(link: impl Fn(&Path, &Path)) -> Result<()> {
    let data = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    let path = {
        let service = ScratchpadStore::new(data.path())?;
        service.lease_workspace(workspace.path())?.path
    };
    fs::create_dir(outside.path().join("nested"))?;
    let sentinel = outside.path().join("nested/sentinel.txt");
    fs::write(&sentinel, "outside must survive")?;
    let modified = fs::metadata(&sentinel)?.modified()?;
    fs::create_dir(path.join("nested"))?;
    link(&path.join("nested/outside"), outside.path());
    age_tree(&path)?;
    // The target remains recent: following it during the age scan would retain
    // scratch incorrectly, while following it during removal would lose data.
    assert_eq!(fs::metadata(&sentinel)?.modified()?, modified);
    assert_eq!(ScratchpadStore::new(data.path())?.sweep_workspace()?, 1);
    assert!(!path.exists());
    assert!(outside.path().join("nested").is_dir());
    assert_eq!(fs::read_to_string(&sentinel)?, "outside must survive");
    assert_eq!(fs::metadata(sentinel)?.modified()?, modified);
    Ok(())
}

#[cfg(unix)]
#[test]
fn sweep_removes_old_scratch_without_following_nested_outside_symlinks() -> Result<()> {
    old_scratch_with_outside_link(|link, target| std::os::unix::fs::symlink(target, link).unwrap())
}

#[cfg(windows)]
#[test]
fn sweep_removes_old_scratch_without_following_nested_outside_junctions() -> Result<()> {
    old_scratch_with_outside_link(|link, target| {
        let ordinary = |path: &Path| {
            path.to_string_lossy()
                .trim_start_matches(r"\\?\")
                .to_owned()
        };
        let output = Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(ordinary(link))
            .arg(ordinary(target))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    })
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn workspace_leases_exclude_sweeping_across_processes() -> Result<()> {
    let data = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let ready = data.path().join("lease-ready");
    let mut child = ChildGuard(
        Command::new(std::env::current_exe()?)
            .args(["--exact", "lease_holder_subprocess", "--nocapture"])
            .env("BUBBLEPAW_SCRATCH_TEST_DATA", data.path())
            .env("BUBBLEPAW_SCRATCH_TEST_WORKSPACE", workspace.path())
            .env("BUBBLEPAW_SCRATCH_TEST_READY", &ready)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()?,
    );
    let started = Instant::now();
    while !ready.exists() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "lease subprocess did not become ready"
        );
        assert!(
            child.0.try_wait()?.is_none(),
            "lease subprocess exited before readiness"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let path = std::path::PathBuf::from(fs::read_to_string(&ready)?);
    age_tree(&path)?;
    let sweeper = ScratchpadStore::new(data.path())?;
    assert_eq!(
        sweeper.sweep_workspace()?,
        0,
        "separate live process owns the shared lease"
    );
    child.0.stdin.take().unwrap().write_all(b"release\n")?;
    assert!(child.0.wait()?.success());
    assert_eq!(
        sweeper.sweep_workspace()?,
        1,
        "process exit releases the lease"
    );
    Ok(())
}

#[test]
fn lease_holder_subprocess() -> Result<()> {
    let Some(data) = std::env::var_os("BUBBLEPAW_SCRATCH_TEST_DATA") else {
        return Ok(());
    };
    let service = ScratchpadStore::new(data)?;
    let lease =
        service.lease_workspace(std::env::var_os("BUBBLEPAW_SCRATCH_TEST_WORKSPACE").unwrap())?;
    let ready = std::path::PathBuf::from(std::env::var_os("BUBBLEPAW_SCRATCH_TEST_READY").unwrap());
    let pending = ready.with_extension("pending");
    fs::write(&pending, lease.path.to_string_lossy().as_bytes())?;
    fs::rename(pending, ready)?;
    std::io::stdin().read_line(&mut String::new())?;
    Ok(())
}
