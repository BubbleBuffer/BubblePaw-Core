//! Local control transport for a running host.
//!
//! The host that holds the store lock listens on one endpoint that only the
//! current user can reach: a Unix socket inside the data directory, or a named
//! pipe under Windows. It then publishes `host.json` (pid, endpoint and protocol
//! version), so clients that start while it is still coming up can find it.
//!
//! The caller holds the store lock for as long as a [`HostEndpoint`] lives. This
//! module neither elects a host nor touches the lock, and it opens no network
//! listener.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};

/// Control protocol version this build speaks. `host.json` records the host's
/// version, and a client refuses a host that reports a different one.
// Version 2 replaced the invocation environment with references; 3 adds `attach.session`.
pub const HOST_PROTOCOL_VERSION: u32 = 3;

/// Metadata file the host publishes in the data directory.
pub const HOST_METADATA_FILE: &str = "host.json";

/// Delay between connection attempts while a host is still publishing.
const CONNECT_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// A bidirectional byte stream to a host or from a client. Split it with
/// `tokio::io::split`.
pub trait HostStream: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> HostStream for T {}

/// One attached client connection.
pub type Connection = Box<dyn HostStream>;

/// Contents of `host.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostMetadata {
    pub pid: u32,
    pub endpoint: String,
    pub protocol_version: u32,
}

/// The listening side of the control transport.
///
/// Dropping it removes this host's `host.json` (only when that file still names
/// this host) and removes its endpoint.
pub struct HostEndpoint {
    data_dir: PathBuf,
    metadata: HostMetadata,
    listener: platform::Listener,
}

impl HostEndpoint {
    /// Listen for the current user only, then publish `host.json` in `data_dir`.
    ///
    /// The caller must already hold the store lock and keep the returned value
    /// alive for as long as clients should be able to attach.
    pub async fn bind(data_dir: &Path) -> Result<Self> {
        let data_dir = std::path::absolute(data_dir)
            .with_context(|| format!("resolve data dir {}", data_dir.display()))?;
        let (listener, endpoint) = platform::bind(&data_dir)?;
        let metadata = HostMetadata {
            pid: std::process::id(),
            endpoint,
            protocol_version: HOST_PROTOCOL_VERSION,
        };
        // If publishing fails, `listener` drops on the way out and removes its endpoint.
        publish_metadata(&data_dir, &metadata)?;
        Ok(Self {
            data_dir,
            metadata,
            listener,
        })
    }

    /// The metadata this host published.
    pub fn metadata(&self) -> &HostMetadata {
        &self.metadata
    }

    /// Wait for the next client that the current user may attach.
    pub async fn accept(&mut self) -> Result<Connection> {
        self.listener.accept().await
    }
}

impl Drop for HostEndpoint {
    fn drop(&mut self) {
        // Remove the metadata only while it still names this host.
        if matches!(read_host_metadata(&self.data_dir), Ok(Some(found)) if found == self.metadata) {
            let _ = fs::remove_file(self.data_dir.join(HOST_METADATA_FILE));
        }
    }
}

/// Read `host.json` from `data_dir`. Returns `Ok(None)` when no host has
/// published, and an error when the file exists but cannot be parsed.
pub fn read_host_metadata(data_dir: &Path) -> Result<Option<HostMetadata>> {
    let path = data_dir.join(HOST_METADATA_FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parse {}", path.display()))
        .map(Some)
}

/// Connect to the host that publishes in `data_dir`.
///
/// Retries for up to `timeout` while `host.json` is absent or its endpoint is
/// not listening yet, which covers a host that is still starting. A host that
/// reports a different protocol version is refused at once.
pub async fn connect(data_dir: &Path, timeout: Duration) -> Result<Connection> {
    let deadline = Instant::now() + timeout;
    // The pid and error of the most recent attempt, for the timeout message.
    let mut last_seen: Option<(u32, String)> = None;
    loop {
        if let Some(meta) = read_host_metadata(data_dir)? {
            if meta.protocol_version != HOST_PROTOCOL_VERSION {
                bail!(
                    "host pid {} speaks protocol version {}, but this client speaks {}",
                    meta.pid,
                    meta.protocol_version,
                    HOST_PROTOCOL_VERSION
                );
            }
            match platform::connect(&meta.endpoint).await {
                Ok(conn) => return Ok(conn),
                Err(err) if platform::is_retryable(&err) => {
                    last_seen = Some((meta.pid, err.to_string()));
                }
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!("connect to host pid {} at {}", meta.pid, meta.endpoint)
                    });
                }
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(CONNECT_POLL_INTERVAL).await;
    }
    match last_seen {
        Some((pid, err)) => bail!(
            "host pid {pid} from host.json did not accept connections within {timeout:?} ({err}); \
             it may have exited without cleaning up"
        ),
        None => bail!(
            "no host.json appeared in {} within {timeout:?}",
            data_dir.display()
        ),
    }
}

/// Write `host.json` atomically: stage it beside the target, then rename over it.
fn publish_metadata(data_dir: &Path, metadata: &HostMetadata) -> Result<()> {
    let target = data_dir.join(HOST_METADATA_FILE);
    let staging = data_dir.join(format!("{HOST_METADATA_FILE}.{}.tmp", metadata.pid));
    let json = serde_json::to_vec_pretty(metadata)?;
    let written = write_then_rename(&staging, &target, &json);
    if written.is_err() {
        let _ = fs::remove_file(&staging);
    }
    written.with_context(|| format!("publish {}", target.display()))
}

fn write_then_rename(staging: &Path, target: &Path, json: &[u8]) -> io::Result<()> {
    let _ = fs::remove_file(staging);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(staging)?;
    file.write_all(json)?;
    file.sync_all()?;
    drop(file);
    fs::rename(staging, target)
}

#[cfg(unix)]
mod platform {
    use std::fs::{self, Permissions};
    use std::io;
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result, bail};
    use tokio::net::{UnixListener, UnixStream};

    use super::Connection;

    /// Socket file inside the data directory.
    const SOCKET_FILE: &str = "host.sock";

    pub(super) struct Listener {
        inner: UnixListener,
        path: PathBuf,
        uid: u32,
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    impl Listener {
        pub(super) async fn accept(&mut self) -> Result<Connection> {
            loop {
                let (stream, _) = self.inner.accept().await.context("accept client")?;
                match stream.peer_cred() {
                    Ok(cred) if cred.uid() == self.uid => return Ok(Box::new(stream)),
                    Ok(cred) => {
                        tracing::warn!(
                            peer_uid = cred.uid(),
                            "dropped control connection from another user"
                        );
                    }
                    Err(err) => {
                        tracing::warn!(%err, "dropped control connection without peer credentials");
                    }
                }
            }
        }
    }

    /// Create the private socket in `data_dir` and return it with its endpoint path.
    pub(super) fn bind(data_dir: &Path) -> Result<(Listener, String)> {
        let uid = effective_uid();
        prepare_dir(data_dir, uid)?;
        let path = data_dir.join(SOCKET_FILE);
        let endpoint = path
            .to_str()
            .with_context(|| format!("socket path {} is not UTF-8", path.display()))?
            .to_owned();
        clear_stale_socket(&path)?;
        let inner =
            UnixListener::bind(&path).with_context(|| format!("listen on {}", path.display()))?;
        // From here on the listener's drop removes the socket if setup fails.
        let listener = Listener {
            inner,
            path: path.clone(),
            uid,
        };
        fs::set_permissions(&path, Permissions::from_mode(0o600))
            .with_context(|| format!("restrict {}", path.display()))?;
        Ok((listener, endpoint))
    }

    pub(super) async fn connect(endpoint: &str) -> io::Result<Connection> {
        let stream = UnixStream::connect(endpoint).await?;
        check_peer_uid(stream.peer_cred()?.uid(), effective_uid())?;
        Ok(Box::new(stream))
    }

    /// Absent socket (or a socket left by a host that died) is retried.
    pub(super) fn is_retryable(err: &io::Error) -> bool {
        matches!(
            err.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
        )
    }

    /// Refuse a peer whose effective user is not the current user.
    pub(super) fn check_peer_uid(peer: u32, expected: u32) -> io::Result<()> {
        if peer == expected {
            return Ok(());
        }
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("endpoint belongs to uid {peer}, not the current user ({expected})"),
        ))
    }

    pub(super) fn effective_uid() -> u32 {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() }
    }

    /// Make `data_dir` a private directory owned by the current user. It is the
    /// socket's parent, so this is what keeps other users away from the socket.
    fn prepare_dir(data_dir: &Path, uid: u32) -> Result<()> {
        fs::create_dir_all(data_dir)
            .with_context(|| format!("create data dir {}", data_dir.display()))?;
        // symlink_metadata does not follow links, so a symlinked data dir is refused.
        let meta = fs::symlink_metadata(data_dir)
            .with_context(|| format!("stat data dir {}", data_dir.display()))?;
        if !meta.is_dir() {
            bail!("data dir {} is not a plain directory", data_dir.display());
        }
        if meta.uid() != uid {
            bail!(
                "data dir {} belongs to uid {}, not the current user ({uid})",
                data_dir.display(),
                meta.uid()
            );
        }
        if meta.mode() & 0o777 != 0o700 {
            fs::set_permissions(data_dir, Permissions::from_mode(0o700))
                .with_context(|| format!("restrict data dir {}", data_dir.display()))?;
        }
        Ok(())
    }

    /// Remove a socket file left by a host that exited without cleaning up. The
    /// store lock makes this safe; anything else at that path is refused.
    fn clear_stale_socket(path: &Path) -> Result<()> {
        match fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_socket() => fs::remove_file(path)
                .with_context(|| format!("remove stale socket {}", path.display())),
            Ok(_) => bail!(
                "{} exists and is not a socket; refusing to replace it",
                path.display()
            ),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).with_context(|| format!("stat {}", path.display())),
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use std::ptr::null_mut;

    use anyhow::{Context, Result};
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_PIPE_BUSY, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows_sys::core::PWSTR;

    use super::Connection;

    pub(super) struct Listener {
        name: String,
        descriptor: SecurityDescriptor,
        /// The instance that the next client will connect to.
        next: Option<NamedPipeServer>,
    }

    /// Create the pipe, restricted to the current user and to local clients.
    pub(super) fn bind(_data_dir: &Path) -> Result<(Listener, String)> {
        let name = format!(r"\\.\pipe\bubblepaw-host-{}", uuid::Uuid::new_v4().simple());
        let descriptor = SecurityDescriptor::current_user()?;
        let first = create_instance(&name, &descriptor, true)
            .with_context(|| format!("create named pipe {name}"))?;
        let endpoint = name.clone();
        Ok((
            Listener {
                name,
                descriptor,
                next: Some(first),
            },
            endpoint,
        ))
    }

    impl Listener {
        pub(super) async fn accept(&mut self) -> Result<Connection> {
            // Keep the pending instance owned by the listener across the await:
            // the host selects accept alongside primary/client completion.
            if self.next.is_none() {
                self.next = Some(create_instance(&self.name, &self.descriptor, false)?);
            }
            self.next
                .as_ref()
                .expect("pending pipe")
                .connect()
                .await
                .context("accept client")?;
            let next = create_instance(&self.name, &self.descriptor, false)?;
            let current = self.next.replace(next).expect("connected pipe");
            Ok(Box::new(current))
        }
    }

    /// Open the pipe that `host.json` names, then refuse it unless the process
    /// serving it runs as the current user. The data directory may be writable by
    /// other local users, so the pipe name alone is not trusted with the request.
    pub(super) async fn connect(endpoint: &str) -> io::Result<Connection> {
        let client = ClientOptions::new().open(endpoint)?;
        verify_server_user(client.as_raw_handle())?;
        Ok(Box::new(client))
    }

    fn verify_server_user(pipe: HANDLE) -> io::Result<()> {
        let refuse = |detail: String| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "refusing to attach: the host pipe is not served by the current user ({detail});                      host.json may point at another user's pipe"
                ),
            )
        };
        let mut pid = 0u32;
        // SAFETY: `pipe` is the open client handle; `pid` receives the server's id.
        if unsafe { GetNamedPipeServerProcessId(pipe, &mut pid) } == 0 {
            return Err(refuse(format!(
                "server process unknown: {}",
                io::Error::last_os_error()
            )));
        }
        // SAFETY: OpenProcess has no pointer arguments; a null result is checked.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return Err(refuse(format!(
                "server process {pid} cannot be inspected: {}",
                io::Error::last_os_error()
            )));
        }
        let process = OwnedHandle(process);
        let server = process_user_sid(process.0)
            .map_err(|error| refuse(format!("server process {pid} user unknown: {error}")))?;
        if server != current_user_sid()? {
            return Err(refuse(format!("server process {pid} runs as {server}")));
        }
        Ok(())
    }

    /// A missing pipe (NotFound) or one with all instances busy is retried.
    pub(super) fn is_retryable(err: &io::Error) -> bool {
        err.kind() == io::ErrorKind::NotFound || err.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
    }

    fn create_instance(
        name: &str,
        descriptor: &SecurityDescriptor,
        first: bool,
    ) -> io::Result<NamedPipeServer> {
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        let mut options = ServerOptions::new();
        options
            .first_pipe_instance(first)
            .reject_remote_clients(true);
        // SAFETY: `attributes` and the descriptor it points to live across the call,
        // and CreateNamedPipe copies the descriptor.
        unsafe {
            options.create_with_security_attributes_raw(
                name,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast::<c_void>(),
            )
        }
    }

    /// A self-relative security descriptor whose only ACE allows the current user.
    struct SecurityDescriptor(*mut c_void);

    // SAFETY: the descriptor is an immutable LocalAlloc buffer, read-only after
    // creation and freed once by `Drop`, so sharing it across threads is sound.
    unsafe impl Send for SecurityDescriptor {}
    // SAFETY: as above; nothing mutates the buffer after creation.
    unsafe impl Sync for SecurityDescriptor {}

    impl SecurityDescriptor {
        fn current_user() -> io::Result<Self> {
            let sid = current_user_sid()?;
            // "D:P" is a protected DACL, so nothing is inherited from the parent.
            let sddl = to_wide(&format!("D:P(A;;GA;;;{sid})"));
            let mut descriptor: *mut c_void = null_mut();
            // SAFETY: `sddl` is NUL-terminated; on success `descriptor` holds a
            // LocalAlloc buffer that `Drop` releases.
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(descriptor))
        }
    }

    impl Drop for SecurityDescriptor {
        fn drop(&mut self) {
            // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW,
            // which documents LocalFree as the release call.
            unsafe { LocalFree(self.0) };
        }
    }

    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: the handle came from OpenProcess or OpenProcessToken and is
            // closed only here.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// The current process user's SID in string form, such as `S-1-5-21-...`.
    fn current_user_sid() -> io::Result<String> {
        // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing.
        process_user_sid(unsafe { GetCurrentProcess() })
    }

    /// The user SID of the process behind `process`, in string form.
    fn process_user_sid(process: HANDLE) -> io::Result<String> {
        let mut raw: HANDLE = null_mut();
        // SAFETY: `process` is a valid process handle; `raw` receives a handle the guard closes.
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut raw) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = OwnedHandle(raw);

        let mut needed = 0u32;
        // SAFETY: a null buffer with zero length only asks for the required size.
        unsafe { GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut needed) };
        // A u64 buffer keeps the TOKEN_USER contents aligned.
        let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
        // SAFETY: the buffer is at least `needed` bytes long.
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: GetTokenInformation wrote a TOKEN_USER at the start of the buffer.
        let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };

        let mut sid_text: PWSTR = null_mut();
        // SAFETY: `user.User.Sid` points into `buffer`, which is still alive.
        if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid_text) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: on success `sid_text` is a NUL-terminated wide string.
        let text = unsafe {
            let len = (0..)
                .take_while(|&offset| *sid_text.add(offset) != 0)
                .count();
            String::from_utf16_lossy(std::slice::from_raw_parts(sid_text, len))
        };
        // SAFETY: ConvertSidToStringSidW allocates with LocalAlloc.
        unsafe { LocalFree(sid_text.cast()) };
        Ok(text)
    }

    fn to_wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// An endpoint that nothing listens on, in the form this platform uses.
    fn missing_endpoint(dir: &Path) -> String {
        if cfg!(windows) {
            r"\\.\pipe\bubblepaw-test-missing".to_owned()
        } else {
            dir.join("missing.sock").to_str().unwrap().to_owned()
        }
    }

    fn write_metadata(dir: &Path, metadata: &HostMetadata) {
        fs::write(
            dir.join(HOST_METADATA_FILE),
            serde_json::to_vec(metadata).unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn bind_publishes_metadata_and_connect_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = HostEndpoint::bind(dir.path()).await.unwrap();
        let published = read_host_metadata(dir.path()).unwrap().unwrap();
        assert_eq!(published, *host.metadata());
        assert_eq!(published.pid, std::process::id());
        assert_eq!(published.protocol_version, HOST_PROTOCOL_VERSION);

        let server = tokio::spawn(async move {
            let conn = host.accept().await.unwrap();
            let (mut reader, mut writer) = tokio::io::split(conn);
            let mut buf = [0u8; 5];
            reader.read_exact(&mut buf).await.unwrap();
            writer.write_all(&buf).await.unwrap();
            writer.flush().await.unwrap();
            host
        });

        let client = connect(dir.path(), Duration::from_secs(5)).await.unwrap();
        let (mut reader, mut writer) = tokio::io::split(client);
        writer.write_all(b"hello").await.unwrap();
        let mut reply = [0u8; 5];
        reader.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"hello");

        let host = server.await.unwrap();
        #[cfg(unix)]
        let socket = dir.path().join("host.sock");
        assert!(read_host_metadata(dir.path()).unwrap().is_some());
        drop(host);
        assert!(read_host_metadata(dir.path()).unwrap().is_none());
        #[cfg(unix)]
        assert!(!socket.exists(), "drop must remove the socket file");
    }

    #[tokio::test]
    async fn cancelling_accept_preserves_the_pending_listener() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = HostEndpoint::bind(dir.path()).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), host.accept())
                .await
                .is_err()
        );
        let mut client = connect(dir.path(), Duration::from_secs(5)).await.unwrap();
        client.write_all(b"ok").await.unwrap();
        let mut server = tokio::time::timeout(Duration::from_secs(5), host.accept())
            .await
            .unwrap()
            .unwrap();
        let mut received = [0; 2];
        server.read_exact(&mut received).await.unwrap();
        assert_eq!(&received, b"ok");
    }

    #[tokio::test]
    async fn connect_waits_for_a_host_that_is_still_publishing() {
        let dir = tempfile::tempdir().unwrap();
        let mut client = std::pin::pin!(connect(dir.path(), Duration::from_secs(5)));
        // Poll once: the first attempt finds no host and the client retries.
        assert!(futures_util::poll!(&mut client).is_pending());
        let host = HostEndpoint::bind(dir.path()).await.unwrap();
        client.await.unwrap();
        drop(host);
    }

    #[tokio::test]
    async fn connect_without_metadata_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let err = connect(dir.path(), Duration::from_millis(100))
            .await
            .err()
            .expect("no host is running");
        assert!(err.to_string().contains("no host.json"), "{err}");
    }

    #[tokio::test]
    async fn stale_metadata_error_names_the_pid() {
        let dir = tempfile::tempdir().unwrap();
        write_metadata(
            dir.path(),
            &HostMetadata {
                pid: 424242,
                endpoint: missing_endpoint(dir.path()),
                protocol_version: HOST_PROTOCOL_VERSION,
            },
        );
        let err = connect(dir.path(), Duration::from_millis(100))
            .await
            .err()
            .expect("the endpoint is gone");
        assert!(err.to_string().contains("pid 424242"), "{err}");
    }

    #[tokio::test]
    async fn protocol_version_mismatch_names_the_pid() {
        let dir = tempfile::tempdir().unwrap();
        write_metadata(
            dir.path(),
            &HostMetadata {
                pid: 515151,
                endpoint: missing_endpoint(dir.path()),
                protocol_version: HOST_PROTOCOL_VERSION + 98,
            },
        );
        let err = connect(dir.path(), Duration::from_secs(5))
            .await
            .err()
            .expect("version differs");
        let text = err.to_string();
        assert!(text.contains("pid 515151"), "{text}");
        assert!(
            text.contains(&(HOST_PROTOCOL_VERSION + 98).to_string()),
            "{text}"
        );
    }

    #[tokio::test]
    async fn corrupt_metadata_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(HOST_METADATA_FILE), b"{ not json").unwrap();
        assert!(read_host_metadata(dir.path()).is_err());
        let err = connect(dir.path(), Duration::from_millis(100))
            .await
            .err()
            .expect("metadata cannot be parsed");
        assert!(err.to_string().contains("host.json"), "{err}");
    }

    #[tokio::test]
    async fn drop_keeps_metadata_that_names_another_host() {
        let dir = tempfile::tempdir().unwrap();
        let host = HostEndpoint::bind(dir.path()).await.unwrap();
        let other = HostMetadata {
            pid: 777,
            endpoint: missing_endpoint(dir.path()),
            protocol_version: HOST_PROTOCOL_VERSION,
        };
        write_metadata(dir.path(), &other);
        drop(host);
        assert_eq!(read_host_metadata(dir.path()).unwrap(), Some(other));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn socket_and_directory_are_user_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        fs::create_dir(&data).unwrap();
        fs::set_permissions(&data, fs::Permissions::from_mode(0o755)).unwrap();

        let host = HostEndpoint::bind(&data).await.unwrap();
        let dir_mode = fs::metadata(&data).unwrap().permissions().mode() & 0o777;
        let socket_mode = fs::metadata(data.join("host.sock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(socket_mode, 0o600);
        drop(host);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stale_socket_is_replaced_but_a_regular_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("host.sock");
        // Dropping a std listener leaves its socket file behind, like a crashed host.
        drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        let host = HostEndpoint::bind(dir.path()).await.unwrap();
        drop(host);
        assert!(!socket.exists());

        fs::write(&socket, b"not a socket").unwrap();
        let err = HostEndpoint::bind(dir.path())
            .await
            .err()
            .expect("regular file must not be replaced");
        assert!(err.to_string().contains("not a socket"), "{err}");
        assert_eq!(fs::read(&socket).unwrap(), b"not a socket");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_publish_removes_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where host.json belongs makes the final rename fail.
        fs::create_dir(dir.path().join(HOST_METADATA_FILE)).unwrap();
        assert!(HostEndpoint::bind(dir.path()).await.is_err());
        assert!(!dir.path().join("host.sock").exists());
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(
            leftovers,
            vec![std::ffi::OsString::from(HOST_METADATA_FILE)]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn overlong_socket_path_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let mut long = dir.path().to_path_buf();
        for _ in 0..4 {
            long.push("a".repeat(40));
        }
        fs::create_dir_all(&long).unwrap();
        assert!(HostEndpoint::bind(&long).await.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn peer_from_another_uid_is_refused() {
        let uid = platform::effective_uid();
        assert!(platform::check_peer_uid(uid, uid).is_ok());
        let err = platform::check_peer_uid(uid.wrapping_add(1), uid).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }
}
