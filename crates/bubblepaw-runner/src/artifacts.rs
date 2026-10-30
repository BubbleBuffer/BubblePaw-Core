//! Session-owned immutable captures. Public references never contain filesystem paths.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const MAX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_METADATA_BYTES: u64 = 4096;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub struct ArtifactStore {
    workspace: PathBuf,
    directory: PathBuf,
}

impl ArtifactStore {
    pub(crate) fn data_dir(&self) -> &Path {
        self.directory
            .parent()
            .expect("artifact storage has a parent")
    }
    pub fn new(workspace: impl AsRef<Path>, directory: impl AsRef<Path>) -> Result<Self> {
        let workspace = workspace
            .as_ref()
            .canonicalize()
            .context("Workspace is unavailable")?;
        if !workspace.is_dir() {
            bail!("Workspace must be a directory");
        }
        fs::create_dir_all(directory.as_ref()).context("Could not create artifact storage")?;
        reject_symlink(directory.as_ref())?;
        let directory = directory.as_ref().canonicalize()?;
        Ok(Self {
            workspace,
            directory,
        })
    }

    pub fn capture(
        &self,
        session: &str,
        path: &str,
        name: Option<&str>,
        image: bool,
    ) -> Result<ArtifactRef> {
        valid_id(session)?;
        let source = self
            .workspace
            .join(path)
            .canonicalize()
            .context("Artifact source is unavailable")?;
        if !source.starts_with(&self.workspace) {
            bail!("Artifact source must be inside the workspace");
        }
        let metadata = fs::symlink_metadata(&source)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            bail!("Artifact source must be a regular file");
        }
        if metadata.len() > MAX_BYTES {
            bail!("Artifact exceeds the 16 MiB limit");
        }
        let mut file = open_regular(&source)?;
        if source.canonicalize()? != source {
            bail!("Artifact source location changed");
        }
        let mut bytes = Vec::new();
        (&mut file).take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BYTES {
            bail!("Artifact exceeds the 16 MiB limit");
        }
        let name = safe_name(name.unwrap_or_else(|| {
            source
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("artifact")
        }));
        let mime = if image {
            image_mime(&bytes).context("Images must be PNG, JPEG, WebP, or GIF")?
        } else {
            file_mime(&name)
        };
        let artifact = ArtifactRef {
            id: Uuid::new_v4().to_string(),
            name,
            mime: mime.to_owned(),
            size: bytes.len() as u64,
        };
        let session_dir = self.session_dir(session, true)?;
        let data_path = session_dir.join(format!("{}.bin", artifact.id));
        let metadata_path = session_dir.join(format!("{}.json", artifact.id));
        let result = (|| -> Result<()> {
            let mut data = tempfile::NamedTempFile::new_in(&session_dir)?;
            data.write_all(&bytes)?;
            data.as_file().sync_all()?;
            data.persist_noclobber(&data_path)?;
            let mut sidecar = tempfile::NamedTempFile::new_in(&session_dir)?;
            serde_json::to_writer(&mut sidecar, &artifact)?;
            sidecar.as_file().sync_all()?;
            sidecar.persist_noclobber(&metadata_path)?;
            Ok(())
        })();
        if let Err(error) = result {
            let _ = fs::remove_file(&data_path);
            let _ = fs::remove_file(&metadata_path);
            return Err(error.context("Could not save artifact"));
        }
        Ok(artifact)
    }

    pub fn read(&self, session: &str, id: &str) -> Result<(ArtifactRef, Vec<u8>)> {
        valid_id(id)?;
        let session_dir = self.session_dir(session, false)?;
        let artifact = read_reference(&session_dir, id)?;
        let bytes = read_regular(&session_dir.join(format!("{id}.bin")), MAX_BYTES)?;
        if bytes.len() as u64 != artifact.size {
            bail!("Artifact data does not match its metadata");
        }
        if artifact.mime.starts_with("image/") && image_mime(&bytes) != Some(artifact.mime.as_str())
        {
            bail!("Artifact image type is invalid");
        }
        Ok((artifact, bytes))
    }

    /// List validated session-owned references in stable identifier order.
    pub fn list(&self, session: &str) -> Result<Vec<ArtifactRef>> {
        valid_id(session)?;
        let directory = self.directory.join(session);
        if !directory.exists() {
            return Ok(Vec::new());
        }
        let directory = self.session_dir(session, false)?;
        let mut artifacts = Vec::new();
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .context("Invalid artifact ID")?;
            valid_id(id)?;
            artifacts.push(read_reference(&directory, id)?);
        }
        artifacts.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(artifacts)
    }

    fn session_dir(&self, session: &str, create: bool) -> Result<PathBuf> {
        valid_id(session)?;
        reject_symlink(&self.directory)?;
        if self.directory.canonicalize()? != self.directory {
            bail!("Artifact storage location changed");
        }
        let directory = self.directory.join(session);
        if create && !directory.exists() {
            match fs::create_dir(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        reject_symlink(&directory)?;
        if !directory.is_dir() || directory.canonicalize()? != directory {
            bail!("Artifact session storage is invalid");
        }
        Ok(directory)
    }
}

fn read_reference(directory: &Path, id: &str) -> Result<ArtifactRef> {
    let bytes = read_regular(&directory.join(format!("{id}.json")), MAX_METADATA_BYTES)?;
    let artifact: ArtifactRef = serde_json::from_slice(&bytes)?;
    if artifact.id != id
        || artifact.name != safe_name(&artifact.name)
        || artifact.size > MAX_BYTES
        || !allowed_mime(&artifact.mime)
    {
        bail!("Artifact metadata is invalid");
    }
    Ok(artifact)
}

fn valid_id(value: &str) -> Result<()> {
    let id = Uuid::parse_str(value).context("Invalid artifact or session ID")?;
    if id.to_string() != value {
        bail!("Artifact and session IDs must be canonical UUIDs");
    }
    Ok(())
}

fn reject_symlink(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        bail!("Artifact storage cannot contain symbolic links");
    }
    Ok(())
}

fn read_regular(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > limit {
        bail!("Artifact file is invalid or oversized");
    }
    let mut file = open_regular(path)?;
    if path.canonicalize()? != path {
        bail!("Artifact file location changed");
    }
    let mut bytes = Vec::new();
    (&mut file).take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("Artifact file is oversized");
    }
    Ok(bytes)
}

fn open_regular(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Open a reparse point itself so a replaced link cannot redirect this read.
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("Artifact file must be regular");
    }
    Ok(file)
}

fn safe_name(name: &str) -> String {
    let mut result: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '"' | ';') {
                '_'
            } else {
                c
            }
        })
        .take(120)
        .collect();
    result = result.trim_matches([' ', '.']).to_owned();
    if result.is_empty() {
        "artifact".to_owned()
    } else {
        result
    }
}

fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn file_mime(name: &str) -> &'static str {
    match Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("txt" | "md" | "csv" | "log") => "text/plain; charset=utf-8",
        Some("pdf") => "application/pdf",
        Some("zip") => "application/zip",
        _ => "application/octet-stream",
    }
}

fn allowed_mime(mime: &str) -> bool {
    matches!(
        mime,
        "image/png"
            | "image/jpeg"
            | "image/gif"
            | "image/webp"
            | "text/plain; charset=utf-8"
            | "application/pdf"
            | "application/zip"
            | "application/octet-stream"
    )
}
