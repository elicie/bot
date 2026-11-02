//! Workspace file confinement and private application file helpers. Port of `server/files.ts`.
//!
//! Every path is opened by walking its segments from `/` with `openat(O_DIRECTORY | O_NOFOLLOW)`
//! relative to the previously opened directory descriptor, so no symbolic link or non-directory
//! component is ever followed. Files are then opened, created and renamed relative to that pinned
//! parent descriptor. (The Node version reached the same lookups through `/proc/self/fd/<fd>/<name>`.)

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

use grok_bot_model::{FileDocument, FileEntry, FileEntryType, FileListing, HttpError, HttpResult};
use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags};
use rustix::io::Errno;

mod collate;

pub const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 5000;

const LINK_ERROR: &str = "Symbolic links and non-directory parents are not allowed.";
const CONFLICT_ERROR: &str = "File changed on disk. Reload it before saving.";

const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY.union(OFlags::DIRECTORY).union(OFlags::NOFOLLOW).union(OFlags::CLOEXEC);
const READ_FLAGS: OFlags = OFlags::RDONLY.union(OFlags::NOFOLLOW).union(OFlags::NONBLOCK).union(OFlags::CLOEXEC);
const CREATE_FLAGS: OFlags = OFlags::WRONLY.union(OFlags::CREATE).union(OFlags::EXCL).union(OFlags::NOFOLLOW).union(OFlags::CLOEXEC);

/// SHA-256 hex digest used for optimistic save conflicts.
pub fn content_hash(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// `isMissing` in files.ts: the error is an OS `ENOENT`.
pub fn is_missing(error: &io::Error) -> bool {
    error.raw_os_error() == Some(Errno::NOENT.raw_os_error())
}

/// Internal failure: either an HTTP error raised on purpose or a raw OS error that the caller maps.
enum Failure {
    Http(HttpError),
    Io(io::Error),
}

impl From<HttpError> for Failure {
    fn from(error: HttpError) -> Self {
        Self::Http(error)
    }
}

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<Errno> for Failure {
    fn from(error: Errno) -> Self {
        Self::Io(error.into())
    }
}

impl Failure {
    fn code(&self) -> Option<Errno> {
        match self {
            Self::Io(error) => error.raw_os_error().map(Errno::from_raw_os_error),
            Self::Http(_) => None,
        }
    }

    fn is_missing(&self) -> bool {
        self.code() == Some(Errno::NOENT)
    }

    fn into_http(self) -> HttpError {
        match self {
            Self::Http(error) => error,
            Self::Io(error) => io_error(error),
        }
    }

    fn into_io(self) -> io::Error {
        match self {
            Self::Http(error) => io::Error::other(error),
            Self::Io(error) => error,
        }
    }
}

/// `directory()` in files.ts: open `absolute` one segment at a time from `/` without following
/// symbolic links, optionally creating missing segments as 0700 directories.
fn directory(absolute: &Path, create: bool) -> Result<OwnedFd, Failure> {
    walk(absolute, create).map_err(|error| match error {
        Errno::LOOP | Errno::NOTDIR => Failure::Http(HttpError::forbidden(LINK_ERROR)),
        error => Failure::from(error),
    })
}

fn walk(absolute: &Path, create: bool) -> Result<OwnedFd, Errno> {
    let bytes = absolute.as_os_str().as_bytes();
    // `path.parse(absolute).root` is empty for a relative path and `open('')` fails with ENOENT.
    if !bytes.starts_with(b"/") {
        return Err(Errno::NOENT);
    }
    let mut current = rustix::fs::open("/", DIRECTORY_FLAGS, Mode::empty())?;
    for segment in bytes.split(|byte| *byte == b'/').filter(|segment| !segment.is_empty()) {
        let segment = OsStr::from_bytes(segment);
        current = match rustix::fs::openat(&current, segment, DIRECTORY_FLAGS, Mode::empty()) {
            Ok(child) => child,
            Err(Errno::NOENT) if create => {
                match rustix::fs::mkdirat(&current, segment, Mode::RWXU) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(error) => return Err(error),
                }
                rustix::fs::openat(&current, segment, DIRECTORY_FLAGS, Mode::empty())?
            }
            Err(error) => return Err(error),
        };
    }
    Ok(current)
}

/// Lexical normalization used by Node's `path.join`/`path.resolve` (`.` and empty segments are
/// dropped, `..` removes the previous segment and cannot climb above `/`).
fn normalize(path: &Path) -> PathBuf {
    let mut absolute = false;
    let mut parts: Vec<&OsStr> = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Prefix(_) => absolute = true,
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.last().is_some_and(|last| *last != OsStr::new("..")) {
                    parts.pop();
                } else if !absolute {
                    parts.push(OsStr::new(".."));
                }
            }
            Component::Normal(part) => parts.push(part),
        }
    }
    let mut normalized = PathBuf::from(if absolute { "/" } else { "" });
    for part in parts {
        normalized.push(part);
    }
    if normalized.as_os_str().is_empty() {
        normalized.push(".");
    }
    normalized
}

/// `path.resolve(input)`.
fn resolve(input: &Path) -> io::Result<PathBuf> {
    if input.is_absolute() { Ok(normalize(input)) } else { Ok(normalize(&std::env::current_dir()?.join(input))) }
}

/// POSIX `path.dirname` and `path.basename` on raw bytes.
fn split_path(path: &Path) -> (PathBuf, OsString) {
    let bytes = path.as_os_str().as_bytes();
    let mut end = bytes.len();
    while end > 1 && bytes[end - 1] == b'/' {
        end -= 1;
    }
    let trimmed = &bytes[..end];
    match trimmed.iter().rposition(|byte| *byte == b'/') {
        None => (PathBuf::from("."), OsString::from_vec(trimmed.to_vec())),
        Some(index) => {
            let mut parent = &trimmed[..index];
            while parent.len() > 1 && parent.ends_with(b"/") {
                parent = &parent[..parent.len() - 1];
            }
            let parent = if parent.is_empty() { b"/".as_slice() } else { parent };
            (PathBuf::from(OsStr::from_bytes(parent)), OsString::from_vec(trimmed[index + 1..].to_vec()))
        }
    }
}

/// Remove a temporary file next to `parent`, ignoring "already gone". Like a JavaScript `finally`,
/// a cleanup failure replaces the operation's own result.
fn remove_temporary<T, E: From<Errno>>(parent: &OwnedFd, name: &OsStr, result: Result<T, E>) -> Result<T, E> {
    match rustix::fs::unlinkat(parent, name, AtFlags::empty()) {
        Ok(()) | Err(Errno::NOENT) => result,
        Err(error) => Err(error.into()),
    }
}

/// Create (0700) or open an application-private directory and return its absolute path.
/// Rejects the filesystem root.
pub fn private_directory(path: &Path) -> HttpResult<PathBuf> {
    let resolved = resolve(path).map_err(io_error)?;
    if resolved == Path::new("/") {
        return Err(HttpError::bad_request("Application data must use its own private directory."));
    }
    let handle = directory(&resolved, true).map_err(Failure::into_http)?;
    rustix::fs::fchmod(&handle, Mode::RWXU).map_err(|error| io_error(error.into()))?;
    Ok(resolved)
}

/// Validate a user-supplied absolute workspace directory and return its canonical path.
pub fn canonical_directory(input: &str) -> HttpResult<PathBuf> {
    if !input.starts_with('/') || input.contains('\0') || input.split('/').any(|part| part == "..") {
        return Err(HttpError::bad_request("Workspace must be an absolute directory path."));
    }
    let resolved = normalize(Path::new(input));
    drop(directory(&resolved, false).map_err(Failure::into_http)?);
    std::fs::canonicalize(&resolved).map_err(io_error)
}

/// Atomically replace `path` with `bytes` (temp file 0600, fsync, rename, fsync parent).
///
/// The parent directory is walked without following links; a symbolic-link or non-directory
/// parent fails with an error that [`io_error`] maps to the 403 the Node server returned.
pub fn atomic_private_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let (parent_path, name) = split_path(path);
    let mut temporary = name.clone();
    temporary.push(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let parent = directory(&parent_path, false).map_err(Failure::into_io)?;
    let result = (|| -> io::Result<()> {
        let mut file = File::from(rustix::fs::openat(&parent, &temporary, CREATE_FLAGS, Mode::RUSR | Mode::WUSR)?);
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        rustix::fs::renameat(&parent, &temporary, &parent, &name)?;
        rustix::fs::fsync(&parent)?;
        Ok(())
    })();
    remove_temporary(&parent, &temporary, result)
}

/// Map an I/O failure to the HTTP error the Node server returned for it.
pub fn io_error(error: std::io::Error) -> HttpError {
    if error.get_ref().is_some_and(|inner| inner.is::<HttpError>()) {
        if let Some(Ok(inner)) = error.into_inner().map(|inner| inner.downcast::<HttpError>()) {
            return *inner;
        }
        return HttpError::new(500, "The operation failed. Check that the workspace and data directory are accessible.");
    }
    match error.kind() {
        std::io::ErrorKind::NotFound => HttpError::not_found("File or directory does not exist."),
        std::io::ErrorKind::PermissionDenied => HttpError::forbidden("File access was denied."),
        _ => HttpError::new(500, "The operation failed. Check that the workspace and data directory are accessible."),
    }
}

/// `text()` in files.ts: size limit, control-byte detection, strict UTF-8 (BOM removed like
/// `TextDecoder`).
fn text(bytes: &[u8]) -> HttpResult<String> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err(HttpError::too_large("Text files are limited to 1 MiB."));
    }
    if bytes.iter().any(|byte| *byte < 9 || (*byte > 13 && *byte < 32)) {
        return Err(HttpError::new(415, "Binary files cannot be edited."));
    }
    let decoded = std::str::from_utf8(bytes).map_err(|_| HttpError::new(415, "Only UTF-8 text files can be edited."))?;
    Ok(decoded.strip_prefix('\u{feff}').unwrap_or(decoded).to_owned())
}

/// Read `name` inside `parent` as an editable text document. Also returns the permission bits.
fn document(parent: &OwnedFd, name: &OsStr, relative: &str) -> Result<(FileDocument, u32), Failure> {
    let file = rustix::fs::openat(parent, name, READ_FLAGS, Mode::empty())?;
    let stat = rustix::fs::fstat(&file)?;
    if FileType::from_raw_mode(stat.st_mode as _) != FileType::RegularFile {
        return Err(HttpError::new(415, "Only regular text files can be edited.").into());
    }
    if stat.st_nlink != 1 {
        return Err(HttpError::forbidden("Hard-linked files are not allowed.").into());
    }
    if stat.st_size > MAX_FILE_BYTES as i64 {
        return Err(HttpError::too_large("Text files are limited to 1 MiB.").into());
    }
    let mut bytes = Vec::with_capacity(stat.st_size as usize);
    File::from(file).take(MAX_FILE_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    let content = text(&bytes)?;
    let document = FileDocument { path: relative.to_owned(), content, hash: content_hash(&bytes) };
    Ok((document, stat.st_mode as u32 & 0o777))
}

/// Hash of `name` inside `parent`, or `""` when it does not exist.
fn current_hash(parent: &OwnedFd, name: &OsStr, relative: &str) -> Result<String, Failure> {
    match document(parent, name, relative) {
        Ok((document, _)) => Ok(document.hash),
        Err(failure) if failure.is_missing() => Ok(String::new()),
        Err(failure) => Err(failure),
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
        if input.contains('\0') || input.contains('\\') || input.starts_with('/') {
            return Err(HttpError::forbidden("Use a workspace-relative path."));
        }
        if (input.is_empty() || input == ".") && allow_root {
            return Ok(String::new());
        }
        if input.is_empty() || input.split('/').any(|part| part.is_empty() || part == "." || part == "..") {
            return Err(HttpError::forbidden("Path traversal is not allowed."));
        }
        Ok(input.to_owned())
    }

    /// `parent()` in files.ts: pin the parent directory of `input` and run `work` on the final name.
    fn parent<T>(&self, input: &str, work: impl FnOnce(&OwnedFd, &OsStr) -> Result<T, Failure>) -> HttpResult<T> {
        let relative = self.relative(input, false)?;
        let (directory_name, name) = match relative.rsplit_once('/') {
            Some((directory_name, name)) => (directory_name, name),
            None => (".", relative.as_str()),
        };
        let parent_path = normalize(&self.root.join(directory_name));
        let handle = directory(&parent_path, false).map_err(Failure::into_http)?;
        work(&handle, OsStr::new(name)).map_err(|failure| match failure.code() {
            Some(Errno::LOOP) => HttpError::forbidden("Symbolic links are not allowed."),
            Some(Errno::NOENT) => HttpError::not_found("File or directory does not exist."),
            Some(Errno::ACCESS | Errno::PERM) => HttpError::forbidden("File access was denied."),
            _ => failure.into_http(),
        })
    }

    pub fn list(&self, input: &str) -> HttpResult<FileListing> {
        let relative = self.relative(input, true)?;
        let target = normalize(&self.root.join(&relative));
        let handle = directory(&target, false).map_err(Failure::into_http)?;
        let listing = (|| -> Result<FileListing, Failure> {
            let mut names = Vec::new();
            for entry in Dir::read_from(&handle)? {
                let entry = entry?;
                let name = entry.file_name().to_bytes();
                if name != b"." && name != b".." {
                    names.push((name.to_vec(), entry.file_type()));
                }
            }
            if names.len() > MAX_DIRECTORY_ENTRIES {
                return Err(HttpError::too_large("This directory has more than 5,000 entries.").into());
            }
            let mut entries = Vec::new();
            for (name, file_type) in names {
                let file_type = match file_type {
                    // Node's `readdir({ withFileTypes })` falls back to `lstat` when d_type is unknown.
                    FileType::Unknown => {
                        let stat = rustix::fs::statat(&handle, name.as_slice(), AtFlags::SYMLINK_NOFOLLOW)?;
                        FileType::from_raw_mode(stat.st_mode as _)
                    }
                    file_type => file_type,
                };
                let entry_type = match file_type {
                    FileType::Directory => FileEntryType::Directory,
                    FileType::RegularFile => FileEntryType::File,
                    _ => continue,
                };
                let name = String::from_utf8_lossy(&name).into_owned();
                let path = if relative.is_empty() { name.clone() } else { format!("{relative}/{name}") };
                entries.push(FileEntry { name, path, entry_type });
            }
            entries.sort_by(|a, b| match (a.entry_type, b.entry_type) {
                (FileEntryType::Directory, FileEntryType::File) => std::cmp::Ordering::Less,
                (FileEntryType::File, FileEntryType::Directory) => std::cmp::Ordering::Greater,
                _ => collate::compare(&a.name, &b.name),
            });
            Ok(FileListing { path: relative.clone(), entries })
        })();
        listing.map_err(|failure| if failure.is_missing() { HttpError::not_found("Directory does not exist.") } else { failure.into_http() })
    }

    pub fn read(&self, input: &str) -> HttpResult<FileDocument> {
        self.parent(input, |parent, name| Ok(document(parent, name, input)?.0))
    }

    /// Hash of the current file contents, or `""` when the file does not exist.
    pub fn expected(&self, input: &str) -> HttpResult<String> {
        self.parent(input, |parent, name| current_hash(parent, name, input))
    }

    /// Save complete contents. `expected_hash` is `""` for a new file.
    pub fn save(&self, input: &str, content: &str, expected_hash: &str) -> HttpResult<FileDocument> {
        // A Rust `&str` is always valid Unicode, so the Node lone-surrogate check (400) cannot fail.
        let bytes = content.as_bytes();
        text(bytes)?;
        self.parent(input, |parent, name| {
            let (actual_hash, mode) = match document(parent, name, input) {
                Ok((document, mode)) => (document.hash, mode),
                Err(failure) if failure.is_missing() => (String::new(), 0o600),
                Err(failure) => return Err(failure),
            };
            if actual_hash != expected_hash {
                return Err(HttpError::conflict(CONFLICT_ERROR).into());
            }
            let temporary = OsString::from(format!(".grok-bot-{}.tmp", uuid::Uuid::new_v4()));
            let result = (|| -> Result<FileDocument, Failure> {
                let mut file = File::from(rustix::fs::openat(parent, &temporary, CREATE_FLAGS, Mode::from_raw_mode(mode as _))?);
                file.write_all(bytes)?;
                file.sync_all()?;
                drop(file);
                if current_hash(parent, name, input)? != expected_hash {
                    return Err(HttpError::conflict(CONFLICT_ERROR).into());
                }
                rustix::fs::renameat(parent, &temporary, parent, name)?;
                rustix::fs::fsync(parent)?;
                Ok(FileDocument { path: input.to_owned(), content: content.to_owned(), hash: content_hash(bytes) })
            })();
            remove_temporary(parent, &temporary, result)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_matches_node_path_semantics() {
        assert_eq!(normalize(Path::new("/a/./b//c/")), Path::new("/a/b/c"));
        assert_eq!(normalize(Path::new("/a/b/../c")), Path::new("/a/c"));
        assert_eq!(normalize(Path::new("/../a")), Path::new("/a"));
        assert_eq!(normalize(Path::new("/")), Path::new("/"));
        assert_eq!(normalize(Path::new("a/../../b")), Path::new("../b"));
    }

    #[test]
    fn split_path_matches_node_dirname_and_basename() {
        assert_eq!(split_path(Path::new("/a/b")), (PathBuf::from("/a"), OsString::from("b")));
        assert_eq!(split_path(Path::new("/a")), (PathBuf::from("/"), OsString::from("a")));
        assert_eq!(split_path(Path::new("/a/b/")), (PathBuf::from("/a"), OsString::from("b")));
        assert_eq!(split_path(Path::new("a")), (PathBuf::from("."), OsString::from("a")));
        assert_eq!(split_path(Path::new("/a//b")), (PathBuf::from("/a"), OsString::from("b")));
    }

    #[test]
    fn text_rejects_binary_and_invalid_utf8_and_strips_bom() {
        assert_eq!(text(b"a\tb\r\n\x0b\x0c\x7f").unwrap(), "a\tb\r\n\x0b\x0c\x7f");
        assert_eq!(text(b"\xef\xbb\xbfhello").unwrap(), "hello");
        for binary in [&b"\x00"[..], b"\x08", b"\x0e", b"\x1f"] {
            assert_eq!(text(binary).unwrap_err(), HttpError::new(415, "Binary files cannot be edited."));
        }
        for invalid in [&b"\xff"[..], b"\xc0\xaf", b"\xed\xa0\x80", b"\xe2\x82"] {
            assert_eq!(text(invalid).unwrap_err(), HttpError::new(415, "Only UTF-8 text files can be edited."));
        }
        assert_eq!(text(&vec![b'x'; MAX_FILE_BYTES + 1]).unwrap_err(), HttpError::too_large("Text files are limited to 1 MiB."));
        assert!(text(&vec![b'x'; MAX_FILE_BYTES]).is_ok());
    }

    #[test]
    fn io_error_unwraps_http_errors_and_maps_os_errors() {
        assert_eq!(io_error(io::Error::other(HttpError::forbidden(LINK_ERROR))), HttpError::forbidden(LINK_ERROR));
        assert_eq!(io_error(Errno::NOENT.into()).status, 404);
        assert_eq!(io_error(Errno::ACCESS.into()), HttpError::forbidden("File access was denied."));
        assert_eq!(io_error(Errno::PERM.into()).status, 403);
        assert_eq!(io_error(Errno::IO.into()).status, 500);
        assert!(is_missing(&Errno::NOENT.into()));
        assert!(!is_missing(&io::Error::from(io::ErrorKind::NotFound)));
    }
}
