//! One process-tree helper: every child starts in an owned tree, and the whole
//! tree dies when its owner kills or drops it.
//!
//! Unix children are placed in their own process group. Windows children belong
//! to a job object with KILL_ON_JOB_CLOSE, so descendants whose original parent
//! already exited are terminated as well.

use imp::Tree;
use std::{
    io,
    sync::{Arc, Mutex},
};
use tokio::process::{Child, Command};

/// A cheaply cloned handle to one child's process tree.
///
/// The tree is killed once, by ProcessTree::kill, or when the last clone is
/// dropped. Later calls do nothing.
#[derive(Clone)]
pub struct ProcessTree(Arc<Tree>);

impl ProcessTree {
    /// Kills every process in this tree. Idempotent.
    pub fn kill(&self) {
        self.0.kill();
    }
}

/// Spawns the command inside a new process tree, so cancelling, timing out or
/// dropping its owner kills the command's descendants too.
///
/// Callers keep setting their own stdio, environment, working directory and
/// kill-on-drop; this only adds tree ownership.
pub fn spawn(command: &mut Command) -> io::Result<(Child, ProcessTree)> {
    imp::spawn(command)
}

#[cfg(unix)]
mod imp {
    use super::*;

    /// The group id is taken before the first signal, so a recycled group id is
    /// never signalled a second time.
    pub(super) struct Tree {
        group: Mutex<Option<i32>>,
    }
    impl Tree {
        pub(super) fn kill(&self) {
            let group = self
                .group
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            let Some(group) = group else {
                return;
            };
            // SAFETY: spawn created this process group exclusively, and the id is
            // consumed here, so the signal can only reach our own members.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
    }
    impl Drop for Tree {
        fn drop(&mut self) {
            self.kill();
        }
    }

    pub(super) fn spawn(command: &mut Command) -> io::Result<(Child, ProcessTree)> {
        // Own process group: a group kill also reaches the children a shell
        // script started, even after the shell itself has exited.
        command.process_group(0);
        let child = command.spawn()?;
        let group = child.id().map(|pid| pid as i32);
        let tree = Tree {
            group: Mutex::new(group),
        };
        Ok((child, ProcessTree(Arc::new(tree))))
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::{
        mem::size_of,
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
        ptr,
    };
    use windows_sys::Win32::{
        Foundation::{HANDLE, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
                Thread32Next,
            },
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
            Threading::{
                CREATE_NO_WINDOW, CREATE_SUSPENDED, GetProcessIdOfThread, OpenThread, ResumeThread,
                THREAD_QUERY_LIMITED_INFORMATION, THREAD_SUSPEND_RESUME, TerminateProcess,
            },
        },
    };

    /// The only job handle is non-inheritable. Closing it kills every member,
    /// including descendants whose original parent has already exited.
    pub(super) struct Tree {
        job: Mutex<Option<OwnedHandle>>,
    }
    impl Tree {
        fn new() -> io::Result<Self> {
            // Null security attributes create a non-inheritable, unnamed handle.
            let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: CreateJobObjectW returned a newly owned valid handle.
            let job = unsafe { OwnedHandle::from_raw_handle(handle) };
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // No breakaway flags: descendants must remain in this job.
            if unsafe {
                SetInformationJobObject(
                    job.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    ptr::from_ref(&limits).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Self {
                job: Mutex::new(Some(job)),
            })
        }

        pub(super) fn kill(&self) {
            // Even a poisoned lock must release ownership during shutdown.
            let job = self
                .job
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            drop(job);
        }
    }
    impl Drop for Tree {
        fn drop(&mut self) {
            self.kill();
        }
    }

    pub(super) fn spawn(command: &mut Command) -> io::Result<(Child, ProcessTree)> {
        let tree = Arc::new(Tree::new()?);
        // Stable Rust does not expose PROC_THREAD_ATTRIBUTE_JOB_LIST. Keeping the
        // initial thread suspended closes the spawn/assign race without replacing
        // std's executable resolution, argv quoting or Tokio's asynchronous pipes.
        command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
        let mut child = command.spawn()?;
        let setup = assign_and_resume(&child, &tree);
        if let Err(error) = setup {
            // Assignment can fail before the job owns the child. Always terminate
            // the original process handle as well as closing the job; never resume
            // or fall back to an unowned process.
            if let Some(handle) = child.raw_handle() {
                unsafe { TerminateProcess(handle, 1) };
            }
            let _ = child.start_kill();
            tree.kill();
            return Err(io::Error::new(
                error.kind(),
                format!("could not own and resume suspended Windows process: {error}"),
            ));
        }
        Ok((child, ProcessTree(tree)))
    }

    fn assign_and_resume(child: &Child, tree: &Tree) -> io::Result<()> {
        let process = child
            .raw_handle()
            .ok_or_else(|| io::Error::other("suspended child process handle unavailable"))?;
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("suspended child process id unavailable"))?;
        let job = tree.job.lock().unwrap_or_else(|error| error.into_inner());
        let handle = job
            .as_ref()
            .ok_or_else(|| io::Error::other("process job already closed"))?;
        if unsafe { AssignProcessToJobObject(handle.as_raw_handle(), process) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let thread = initial_thread(pid)?;
        // CREATE_SUSPENDED promises exactly one suspension on the initial thread.
        // Any unexpected state fails closed, even if this call briefly resumed it:
        // it is already job-owned, so all descendants are still terminated.
        match unsafe { ResumeThread(thread.as_raw_handle()) } {
            1 => Ok(()),
            u32::MAX => Err(io::Error::last_os_error()),
            count => Err(io::Error::other(format!(
                "unexpected initial thread suspension count {count}"
            ))),
        }
    }

    fn initial_thread(pid: u32) -> io::Result<OwnedHandle> {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the successful snapshot call returned a newly owned handle.
        let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
        let mut entry = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..THREADENTRY32::default()
        };
        let mut found = None;
        let mut present = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) };
        while present != 0 {
            if entry.th32OwnerProcessID == pid {
                if found.is_some() {
                    return Err(io::Error::other("suspended child has multiple threads"));
                }
                let thread: HANDLE = unsafe {
                    OpenThread(
                        THREAD_SUSPEND_RESUME | THREAD_QUERY_LIMITED_INFORMATION,
                        0,
                        entry.th32ThreadID,
                    )
                };
                if thread.is_null() {
                    return Err(io::Error::last_os_error());
                }
                // SAFETY: OpenThread returned a newly owned valid handle.
                let thread = unsafe { OwnedHandle::from_raw_handle(thread) };
                if unsafe { GetProcessIdOfThread(thread.as_raw_handle()) } != pid {
                    return Err(io::Error::other("initial thread owner changed"));
                }
                found = Some(thread);
            }
            entry.dwSize = size_of::<THREADENTRY32>() as u32;
            present = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) };
        }
        found.ok_or_else(|| io::Error::other("suspended child initial thread not found"))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        process::Stdio,
        time::{Duration, Instant},
    };
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
        process::ChildStdout,
    };

    /// Starts a shell that puts a grandchild in the background and prints its
    /// pid, so a test can prove that the whole tree dies.
    async fn grandchild() -> (i32, Child, ProcessTree, Lines<BufReader<ChildStdout>>) {
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "sleep 30 & echo $!; read -r line; echo \"$line\"; wait",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let (mut child, tree) = spawn(&mut command).expect("spawn the shell");
        let stdout = child.stdout.take().expect("stdout pipe");
        let mut lines = BufReader::new(stdout).lines();
        let pid: i32 = lines
            .next_line()
            .await
            .expect("read the grandchild pid")
            .expect("grandchild pid line")
            .trim()
            .parse()
            .expect("numeric grandchild pid");
        (pid, child, tree, lines)
    }

    fn alive(pid: i32) -> bool {
        #[cfg(target_os = "linux")]
        {
            match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                // A zombie has already been killed; only its exit status waits
                // for a reaper that is not ours.
                Ok(stat) => !stat
                    .rsplit(')')
                    .next()
                    .unwrap_or_default()
                    .trim_start()
                    .starts_with('Z'),
                Err(_) => false,
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            // SAFETY: signal 0 only probes for the pid's existence.
            unsafe { libc::kill(pid, 0) == 0 }
        }
    }

    async fn gone_within(pid: i32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if !alive(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    #[tokio::test]
    async fn kill_terminates_a_grandchild() {
        let (pid, mut child, tree, _) = grandchild().await;
        assert!(alive(pid), "grandchild {pid} did not start");
        tree.kill();
        assert!(gone_within(pid).await, "grandchild {pid} survived");
        // A later kill is a no-op rather than an error.
        tree.kill();
        let _ = child.wait().await;
    }

    #[tokio::test]
    async fn dropping_the_last_clone_terminates_a_grandchild() {
        let (pid, mut child, tree, mut lines) = grandchild().await;
        let clone = tree.clone();
        drop(tree);
        // A live clone keeps the tree alive. A group SIGKILL is pending on every
        // member before kill(2) returns, so the shell echoing a line after the
        // drop proves no kill was sent.
        let mut stdin = child.stdin.take().expect("stdin pipe");
        stdin
            .write_all(b"still here\n")
            .await
            .expect("write to the shell");
        let echoed = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .expect("the shell answered")
            .expect("read the echo");
        assert_eq!(echoed.as_deref(), Some("still here"));
        assert!(alive(pid), "grandchild {pid} died with a live clone");
        drop(clone);
        assert!(gone_within(pid).await, "grandchild {pid} survived");
        let _ = child.wait().await;
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::{
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::Stdio,
        time::{Duration, Instant},
    };
    use tokio::io::AsyncReadExt;
    use windows_sys::Win32::{
        Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
    };

    /// PowerShell writes its redirected stdout as UTF-16LE or as bytes, and the
    /// grandchild shares that pipe, so a test only reads complete numeric lines.
    fn complete_pid(bytes: &[u8]) -> Option<u32> {
        let zeros = bytes.iter().filter(|byte| **byte == 0).count();
        let text = if zeros * 4 > bytes.len() {
            let units = bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Vec<u16>>();
            String::from_utf16_lossy(&units)
        } else {
            String::from_utf8_lossy(bytes).into_owned()
        };
        let complete = &text[..text.rfind('\n')?];
        complete
            .lines()
            .map(|line| line.trim().trim_start_matches('\u{feff}'))
            .find_map(|line| line.parse::<u32>().ok())
    }

    /// Reads the child's stdout until a complete line is a pid.
    async fn read_pid(mut stdout: tokio::process::ChildStdout) -> u32 {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 512];
        loop {
            let read = stdout
                .read(&mut chunk)
                .await
                .expect("read the grandchild pid");
            assert!(
                read > 0,
                "PowerShell exited without a pid: {:?}",
                String::from_utf8_lossy(&buf)
            );
            buf.extend_from_slice(&chunk[..read]);
            if let Some(pid) = complete_pid(&buf) {
                return pid;
            }
        }
    }

    /// Starts PowerShell, which starts a long-running grandchild and prints its
    /// pid, so a test can prove that the whole tree dies.
    async fn grandchild() -> (u32, Child, ProcessTree) {
        let mut command = Command::new("powershell.exe");
        command
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                // [Console]::Out keeps the pipe in the console code page; a
                // redirected Write-Output would be UTF-16.
                "$ErrorActionPreference = 'Stop'; \
                 $p = Start-Process -FilePath \"$env:SystemRoot\\System32\\PING.EXE\" \
                 -ArgumentList '-n','30','127.0.0.1' -PassThru -NoNewWindow; \
                 [Console]::Out.WriteLine($p.Id)",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let (mut child, tree) = spawn(&mut command).expect("spawn powershell");
        let stdout = child.stdout.take().expect("stdout pipe");
        let pid = read_pid(stdout).await;
        (pid, child, tree)
    }

    /// A held process handle, so a reused pid can never look like a survivor.
    fn process_handle(pid: u32) -> Option<OwnedHandle> {
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            return None;
        }
        // SAFETY: OpenProcess returned a newly owned valid handle.
        Some(unsafe { OwnedHandle::from_raw_handle(handle) })
    }

    fn alive(handle: &OwnedHandle) -> bool {
        // SAFETY: the handle stays valid while it is held here.
        unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) == WAIT_TIMEOUT }
    }

    async fn gone_within(pid: u32) -> bool {
        let handle = match process_handle(pid) {
            Some(handle) => handle,
            None => return true,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            // SAFETY: the handle stays valid while it is held here.
            if unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } == WAIT_OBJECT_0 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    #[tokio::test]
    async fn kill_terminates_a_grandchild() {
        let (pid, mut child, tree) = grandchild().await;
        let handle = process_handle(pid).expect("open the grandchild process");
        assert!(alive(&handle), "grandchild {pid} did not start");
        tree.kill();
        assert!(gone_within(pid).await, "grandchild {pid} survived");
        // A later kill is a no-op rather than an error.
        tree.kill();
        let _ = child.wait().await;
    }

    #[tokio::test]
    async fn dropping_the_last_clone_terminates_a_grandchild() {
        let (pid, mut child, tree) = grandchild().await;
        let handle = process_handle(pid).expect("open the grandchild process");
        let clone = tree.clone();
        drop(tree);
        // A live job handle keeps the tree alive.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(alive(&handle), "grandchild {pid} died with a live clone");
        drop(clone);
        assert!(gone_within(pid).await, "grandchild {pid} survived");
        let _ = child.wait().await;
    }
}
