//! Workspace file confinement and private application file helpers. Port of `server/files.ts`.
//!
//! CONTRACT (owned by the files agent): keep these public signatures. The helper bodies below are
//! provisional std::fs implementations so other crates can build and test; the files agent replaces
//! them with the descriptor-walking, no-follow versions that match the Node implementation.

use std::path::{Path, PathBuf};

use grok_bot_model::{FileDocument, FileListing, HttpError, HttpResult};

pub const MAX_FILE_BYTES: usize = 1024 * 1024;

/// SHA-256 hex digest used for optimistic save conflicts.
pub fn content_hash(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// Create (0700) or open an application-private directory and return its absolute path.
/// Rejects the filesystem root.
pub fn private_directory(path: &Path) -> HttpResult<PathBuf> {
    // PROVISIONAL
    use std::os::unix::fs::PermissionsExt;
    let resolved = std::path::absolute(path).map_err(io_error)?;
    if resolved.parent().is_none() {
        return Err(HttpError::bad_request("Application data must use its own private directory."));
    }
    std::fs::create_dir_all(&resolved).map_err(io_error)?;
    std::fs::set_permissions(&resolved, std::fs::Permissions::from_mode(0o700)).map_err(io_error)?;
    Ok(resolved)
}

/// Validate a user-supplied absolute workspace directory and return its canonical path.
pub fn canonical_directory(input: &str) -> HttpResult<PathBuf> {
    // PROVISIONAL
    let path = Path::new(input);
    if !path.is_absolute() || input.contains('\0') || path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err(HttpError::bad_request("Workspace must be an absolute directory path."));
    }
    let canonical = std::fs::canonicalize(path).map_err(io_error)?;
    if !canonical.is_dir() {
        return Err(HttpError::forbidden("Symbolic links and non-directory parents are not allowed."));
    }
    Ok(canonical)
}

/// Atomically replace `path` with `bytes` (temp file 0600, fsync, rename, fsync parent).
pub fn atomic_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // PROVISIONAL
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let temporary = path.with_file_name(format!(
        "{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("state"),
        uuid::Uuid::new_v4()
    ));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        if let Some(parent) = path.parent() {
            std::fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    })();
    let _ = std::fs::remove_file(&temporary);
    result
}

/// Map an I/O failure to the HTTP error the Node server returned for it.
pub fn io_error(error: std::io::Error) -> HttpError {
    match error.kind() {
        std::io::ErrorKind::NotFound => HttpError::not_found("File or directory does not exist."),
        std::io::ErrorKind::PermissionDenied => HttpError::forbidden("File access was denied."),
        _ => HttpError::new(500, "The operation failed. Check that the workspace and data directory are accessible."),
    }
}

/// Confined access to one user-selected workspace directory.
pub struct WorkspaceFiles {
    root: PathBuf,
}

impl WorkspaceFiles {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Validate a workspace-relative path (`allow_root` permits `""`/`"."`).
    pub fn relative(&self, input: &str, allow_root: bool) -> HttpResult<String> {
        let _ = (input, allow_root);
        todo!("files agent")
    }

    pub fn list(&self, input: &str) -> HttpResult<FileListing> {
        let _ = input;
        todo!("files agent")
    }

    pub fn read(&self, input: &str) -> HttpResult<FileDocument> {
        let _ = input;
        todo!("files agent")
    }

    /// Hash of the current file contents, or `""` when the file does not exist.
    pub fn expected(&self, input: &str) -> HttpResult<String> {
        let _ = input;
        todo!("files agent")
    }

    /// Save complete contents. `expected_hash` is `""` for a new file.
    pub fn save(&self, input: &str, content: &str, expected_hash: &str) -> HttpResult<FileDocument> {
        let _ = (input, content, expected_hash);
        todo!("files agent")
    }
}
