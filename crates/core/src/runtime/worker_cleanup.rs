//! Blocking garbage collection of already-durably-pruned worker conversations.
//!
//! The caller must plan pruning under the runtime queue, await the successful durable state mutation,
//! and snapshot *all* retained conversation IDs (including ordinary/active parent conversations).
//! Selected-ID cleanup may release the queue before filesystem work: pruned UUIDs cannot be reused.
//! Orphan sweeping must instead run at quiescent startup, before admitting runs or conversations.
//! This module does not decide retention or touch attachments. Startup sweeping recovers a crash
//! between durable pruning and cleanup, even if the private cwd has already been removed.

use std::collections::HashSet;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::ptr::NonNull;

use uuid::Uuid;

const MAX_CWD_BYTES: usize = 8192;
const MAX_REMOVE_DEPTH: usize = 128;
const DIRECTORY_FLAGS: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

/// Remove only selected, unretained UUID caches and their app-private engine cwd directories.
///
/// With `sweep_orphans`, any canonical UUID absent from `retained_conversation_ids` is eligible;
/// otherwise only canonical UUIDs in `removed_ids` are eligible. Invalid IDs/layouts, foreign cwds,
/// symlink entries, and untrusted `.cwd` metadata are ignored. Root/real filesystem failures return
/// the first error, after trying other independent candidates. Missing cache/session directories
/// are normal. Log failures and retry at startup rather than disabling workers.
///
/// No deletion may precede the caller's successful durable prune and queue-guarded retained-ID
/// snapshot. Use `spawn_blocking`; selected-ID cleanup can release the queue first because pruned
/// UUIDs cannot be reused. `sweep_orphans` is only safe at startup before any new admissions.
pub(super) fn cleanup(
    data_dir: &Path,
    engine_root: &Path,
    removed_ids: &[String],
    retained_conversation_ids: &HashSet<String>,
    sweep_orphans: bool,
) -> io::Result<()> {
    // Protect UUID identity even if a retained ID happens to use a noncanonical spelling.
    let retained: HashSet<Uuid> = retained_conversation_ids.iter().filter_map(|id| Uuid::parse_str(id).ok()).collect();
    let selected: HashSet<Uuid> = removed_ids.iter().filter_map(|id| canonical_uuid(id)).filter(|id| !retained.contains(id)).collect();
    let eligible = Eligibility { selected, retained, sweep_orphans };
    if !sweep_orphans && eligible.selected.is_empty() {
        return Ok(());
    }

    let data_path = absolute_root(data_dir)?;
    let data = Directory::open_absolute(&data_path)
        .map_err(|error| at_path("open cleanup data root", &data_path, error))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Cleanup data root is missing."))?;
    // Open without following *any* ancestor first; canonicalize must not legitimize a symlink root.
    let canonical = std::fs::canonicalize(&data_path)?;
    if canonical.as_os_str() != data_path.as_os_str() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Cleanup data root is not canonical."));
    }

    let mut first_error = None;
    let cache = keep_error(data.child_directory(OsStr::new("grok")), &mut first_error).flatten();
    let sessions =
        keep_error(absolute_root(engine_root).and_then(|root| Directory::open_absolute(&root.join("home/.grok/sessions"))), &mut first_error).flatten();

    // Check any still-existing cwd ancestors before removing caches. Missing private cwds are fine,
    // but an existing symlink/non-directory must not establish ownership of an engine cwd folder.
    if let Some(sessions) = sessions {
        let _ = keep_error(cleanup_sessions(&data, &canonical, &sessions, &eligible), &mut first_error);
    }
    if let Some(cache) = cache {
        let _ = keep_error(cleanup_caches(&cache, &eligible), &mut first_error);
    }
    finish(first_error)
}

struct Eligibility {
    selected: HashSet<Uuid>,
    retained: HashSet<Uuid>,
    sweep_orphans: bool,
}

impl Eligibility {
    fn permits(&self, id: &Uuid) -> bool {
        !self.retained.contains(id) && (self.sweep_orphans || self.selected.contains(id))
    }
}

fn canonical_uuid(id: &str) -> Option<Uuid> {
    let uuid = Uuid::parse_str(id).ok()?;
    (uuid.hyphenated().to_string() == id).then_some(uuid)
}

fn cleanup_caches(cache: &Directory, eligible: &Eligibility) -> io::Result<()> {
    let mut first_error = None;
    if eligible.sweep_orphans {
        for entry in cache.entries()? {
            let Some(name) = keep_error(entry, &mut first_error) else { continue };
            let Some(id) = name.to_str().and_then(canonical_uuid) else { continue };
            if eligible.permits(&id) {
                let _ = keep_error(remove_candidate(cache, &name), &mut first_error);
            }
        }
    } else {
        // Pre-spawn cleanup never inspects unrelated conversation cache directories.
        for id in &eligible.selected {
            let _ = keep_error(remove_candidate(cache, OsStr::new(&id.to_string())), &mut first_error);
        }
    }
    finish(first_error)
}

fn cleanup_sessions(data: &Directory, data_path: &Path, sessions: &Directory, eligible: &Eligibility) -> io::Result<()> {
    let mut first_error = None;
    // Only the cwd-level entries in this application's explicit private engine home are scanned.
    // Session UUIDs beneath a verified cwd folder do not need parsing: the whole folder is owned.
    for entry in sessions.entries()? {
        let Some(name) = keep_error(entry, &mut first_error) else { continue };
        let Some(Some(directory)) = keep_error(sessions.entry_directory(&name), &mut first_error) else { continue };
        let Some(Some(cwd)) = keep_error(decode_cwd(&directory, &name), &mut first_error) else { continue };
        let Some((id, fingerprint)) = owned_cwd(&cwd, data_path) else { continue };
        if !eligible.permits(&id) {
            continue;
        }
        let Some(true) = keep_error(existing_cwd_confined(data, &id, fingerprint), &mut first_error) else { continue };
        let _ = keep_error(
            remove_tree(sessions, &name, &directory, 0).map_err(|error| at_path("remove engine cwd cache", &directory.path, error)),
            &mut first_error,
        );
    }
    finish(first_error)
}

/// Compare raw lexical spelling, not `Path` equality (which normalizes `.` and repeated `/`).
fn owned_cwd<'a>(cwd: &'a str, data_path: &Path) -> Option<(Uuid, &'a str)> {
    let prefix = format!("{}/grok/", data_path.to_str()?);
    let mut parts = cwd.strip_prefix(prefix.as_str())?.split('/');
    let id = canonical_uuid(parts.next()?)?;
    let fingerprint = parts.next()?;
    if fingerprint.len() != 64 || !fingerprint.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) {
        return None;
    }
    (parts.next()? == "workspace" && parts.next().is_none()).then_some((id, fingerprint))
}

fn existing_cwd_confined(data: &Directory, id: &Uuid, fingerprint: &str) -> io::Result<bool> {
    let id = id.to_string();
    let mut current = None;
    for name in ["grok", &id, fingerprint, "workspace"] {
        let parent = current.as_ref().unwrap_or(data);
        match parent.child_directory(OsStr::new(name)) {
            Ok(Some(child)) => current = Some(child),
            Ok(None) => return Ok(true), // Orphan sessions remain recognizable after the cwd is gone.
            Err(error) if unsafe_entry(&error) => return Ok(false),
            Err(error) => return Err(error),
        }
    }
    Ok(true)
}

fn decode_cwd(directory: &Directory, name: &OsStr) -> io::Result<Option<String>> {
    let Some(name) = name.to_str() else { return Ok(None) };
    let Some(decoded) = percent_decode(name) else { return Ok(None) };
    if decoded.starts_with('/') {
        return Ok(Some(decoded));
    }
    // configure() always uses the leaf `workspace`. Upstream's long encoding is workspace-<hash16>.
    // No dependency on upstream or blake3 is needed: bounded metadata must still pass owned_cwd().
    let Some(hash) = name.strip_prefix("workspace-") else { return Ok(None) };
    if hash.len() != 16 || !hash.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) {
        return Ok(None);
    }
    let file = match directory.open_file(OsStr::new(".cwd"), libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound || unsafe_entry(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_CWD_BYTES as u64 {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    file.take((MAX_CWD_BYTES + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_CWD_BYTES {
        return Ok(None);
    }
    Ok(String::from_utf8(bytes).ok().map(|cwd| cwd.trim().to_owned()))
}

fn percent_decode(value: &str) -> Option<String> {
    fn digit(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }
    let mut bytes = value.bytes();
    let mut decoded = Vec::with_capacity(value.len());
    while let Some(byte) = bytes.next() {
        decoded.push(if byte == b'%' { digit(bytes.next()?)? * 16 + digit(bytes.next()?)? } else { byte });
    }
    String::from_utf8(decoded).ok()
}

fn absolute_root(path: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::RootDir | Component::Normal(_) => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "Cleanup roots cannot contain parent traversal.")),
        }
    }
    if normalized == Path::new("/") {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Cleanup requires a private application directory."));
    }
    Ok(normalized)
}

/// All opens, metadata reads, enumeration and removals are anchored to no-follow directory FDs.
/// Checking paths and then calling path-based remove_dir_all would leave an ancestor-swap escape.
struct Directory {
    file: File,
    path: PathBuf,
}

impl Directory {
    fn open_absolute(path: &Path) -> io::Result<Option<Self>> {
        let mut current = Self { file: OpenOptions::new().read(true).custom_flags(DIRECTORY_FLAGS).open("/")?, path: PathBuf::from("/") };
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => match current.child_directory(name)? {
                    Some(child) => current = child,
                    None => return Ok(None),
                },
                _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "Cleanup path is not absolute and normalized.")),
            }
        }
        Ok(Some(current))
    }

    fn open_file(&self, name: &OsStr, flags: libc::c_int) -> io::Result<File> {
        let name = entry_name(name)?;
        // SAFETY: the live parent FD and NUL-terminated single component outlive this call. No
        // creation flags are used, and ownership of a successful new FD transfers exactly once.
        let fd = unsafe { libc::openat(self.file.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 { Err(io::Error::last_os_error()) } else { Ok(unsafe { File::from_raw_fd(fd) }) }
    }

    fn child_directory(&self, name: &OsStr) -> io::Result<Option<Self>> {
        match self.open_file(name, DIRECTORY_FLAGS) {
            Ok(file) => Ok(Some(Self { file, path: self.path.join(name) })),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn entry_directory(&self, name: &OsStr) -> io::Result<Option<Self>> {
        match self.child_directory(name) {
            Err(error) if unsafe_entry(&error) => Ok(None), // Candidate symlinks/files are never followed.
            result => result,
        }
    }

    fn entries(&self) -> io::Result<Entries> {
        // Opening `.` creates an independent directory-stream offset, unlike dup(parent_fd).
        // It is the only deliberately permitted dot component, never derived from external input.
        let dot = c".";
        // SAFETY: dot is a valid C string and the parent FD remains live.
        let fd = unsafe { libc::openat(self.file.as_raw_fd(), dot.as_ptr(), DIRECTORY_FLAGS) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is a fresh owned directory FD; fdopendir takes ownership only on success.
        let stream = unsafe { libc::fdopendir(fd) };
        match NonNull::new(stream) {
            Some(stream) => Ok(Entries { stream, done: false }),
            None => {
                let error = io::Error::last_os_error();
                // SAFETY: fdopendir failed, so this FD still belongs to us.
                drop(unsafe { File::from_raw_fd(fd) });
                Err(error)
            }
        }
    }

    fn stat(&self, name: &OsStr) -> io::Result<Option<libc::stat>> {
        let name = entry_name(name)?;
        let mut metadata = MaybeUninit::<libc::stat>::uninit();
        // SAFETY: fstatat initializes metadata on success; both pointers and the parent FD are live.
        let result = unsafe { libc::fstatat(self.file.as_raw_fd(), name.as_ptr(), metadata.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
        if result == 0 {
            Ok(Some(unsafe { metadata.assume_init() }))
        } else {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::NotFound { Ok(None) } else { Err(error) }
        }
    }

    fn still_named(&self, parent: &Directory, name: &OsStr) -> io::Result<bool> {
        let Some(named) = parent.stat(name)? else { return Ok(false) };
        let opened = self.file.metadata()?;
        if named.st_mode & libc::S_IFMT != libc::S_IFDIR || named.st_dev != opened.dev() || named.st_ino != opened.ino() {
            return Err(io::Error::other("Cleanup directory changed during validation."));
        }
        Ok(true)
    }

    fn unlink(&self, name: &OsStr, directory: bool) -> io::Result<()> {
        let name = entry_name(name)?;
        // SAFETY: deletion is relative to a pinned parent and a validated single component. unlinkat
        // never follows a final symlink, with or without AT_REMOVEDIR.
        let result = unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), if directory { libc::AT_REMOVEDIR } else { 0 }) };
        if result == 0 {
            Ok(())
        } else {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::NotFound { Ok(()) } else { Err(error) }
        }
    }
}

fn entry_name(name: &OsStr) -> io::Result<CString> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Invalid cleanup directory entry."));
    }
    CString::new(bytes).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Invalid cleanup directory entry."))
}

struct Entries {
    stream: NonNull<libc::DIR>,
    done: bool,
}

impl Iterator for Entries {
    type Item = io::Result<OsString>;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.done {
            if let Err(error) = clear_errno() {
                self.done = true;
                return Some(Err(error));
            }
            // SAFETY: this stream is exclusively owned until Drop. Copy the name before readdir
            // can invalidate its storage. POSIX guarantees a NUL-terminated d_name.
            let entry = unsafe { libc::readdir(self.stream.as_ptr()) };
            if entry.is_null() {
                let error = io::Error::last_os_error();
                self.done = true;
                return if error.raw_os_error() == Some(0) { None } else { Some(Err(error)) };
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if name != b"." && name != b".." {
                return Some(Ok(OsString::from_vec(name.to_vec())));
            }
        }
        None
    }
}

impl Drop for Entries {
    fn drop(&mut self) {
        // SAFETY: fdopendir transferred exclusive ownership; closedir releases its FD exactly once.
        unsafe { libc::closedir(self.stream.as_ptr()) };
    }
}

fn clear_errno() -> io::Result<()> {
    // readdir uses NULL for both EOF and errors. errno is thread-local; reset it immediately before
    // each call so directory read errors are not silently mistaken for a successful sweep.
    #[cfg(any(target_os = "linux", target_os = "hurd", target_os = "dragonfly", target_os = "emscripten"))]
    let errno = unsafe { libc::__errno_location() };
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos", target_os = "watchos", target_os = "visionos", target_os = "freebsd"))]
    let errno = unsafe { libc::__error() };
    #[cfg(any(target_os = "android", target_os = "netbsd", target_os = "openbsd"))]
    let errno = unsafe { libc::__errno() };
    #[cfg(any(target_os = "solaris", target_os = "illumos"))]
    let errno = unsafe { libc::___errno() };
    #[cfg(any(
        target_os = "linux",
        target_os = "hurd",
        target_os = "dragonfly",
        target_os = "emscripten",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos",
        target_os = "freebsd",
        target_os = "android",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "solaris",
        target_os = "illumos"
    ))]
    {
        // SAFETY: the platform C library supplies this thread's live errno slot.
        unsafe { *errno = 0 };
        Ok(())
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "hurd",
        target_os = "dragonfly",
        target_os = "emscripten",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos",
        target_os = "freebsd",
        target_os = "android",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "solaris",
        target_os = "illumos"
    )))]
    Err(io::Error::new(io::ErrorKind::Unsupported, "Safe cleanup directory enumeration is unavailable on this platform."))
}

fn unsafe_entry(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR))
}

fn remove_candidate(parent: &Directory, name: &OsStr) -> io::Result<()> {
    let Some(directory) = parent.entry_directory(name)? else { return Ok(()) };
    remove_tree(parent, name, &directory, 0).map_err(|error| at_path("remove conversation cache", &directory.path, error))
}

fn remove_tree(parent: &Directory, name: &OsStr, directory: &Directory, depth: usize) -> io::Result<()> {
    if depth >= MAX_REMOVE_DEPTH {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Cleanup directory nesting limit exceeded."));
    }
    if !directory.still_named(parent, name)? {
        return Ok(());
    }
    let mut first_error = None;
    for entry in directory.entries()? {
        let Some(child_name) = keep_error(entry, &mut first_error) else { continue };
        let Some(Some(metadata)) = keep_error(directory.stat(&child_name), &mut first_error) else { continue };
        if metadata.st_mode & libc::S_IFMT == libc::S_IFDIR {
            if let Some(Some(child)) = keep_error(directory.child_directory(&child_name), &mut first_error) {
                let _ = keep_error(remove_tree(directory, &child_name, &child, depth + 1), &mut first_error);
            }
        } else {
            // Interior symlinks are only unlinked, never traversed (including links out of the app).
            let _ = keep_error(directory.unlink(&child_name, false), &mut first_error);
        }
    }
    if keep_error(directory.still_named(parent, name), &mut first_error) == Some(true) {
        let _ = keep_error(parent.unlink(name, true), &mut first_error);
    }
    finish(first_error)
}

fn keep_error<T>(result: io::Result<T>, first_error: &mut Option<io::Error>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            if first_error.is_none() {
                *first_error = Some(error);
            }
            None
        }
    }
}

fn finish(first_error: Option<io::Error>) -> io::Result<()> {
    first_error.map_or(Ok(()), Err)
}

fn at_path(action: &str, path: &Path, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("Cannot {action} at {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    const REMOVED: &str = "11111111-1111-4111-8111-111111111111";
    const RETAINED: &str = "22222222-2222-4222-8222-222222222222";
    const ORPHAN: &str = "33333333-3333-4333-8333-333333333333";
    const PARENT: &str = "44444444-4444-4444-8444-444444444444";
    const SESSION: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const HASH_NAME: &str = "workspace-0123456789abcdef";

    struct Fixture {
        _temp: TempDir,
        base: PathBuf,
        data: PathBuf,
        engine: PathBuf,
    }

    impl Fixture {
        fn new(long: bool) -> Self {
            let temp = tempfile::Builder::new().prefix("gc").tempdir().unwrap();
            // Some platforms put temporary directories beneath a symlink (e.g. /var on macOS).
            let base = fs::canonicalize(temp.path()).unwrap();
            let data = if long { base.join("long-root-".repeat(16)).join("data") } else { base.join("d") };
            fs::create_dir_all(&data).unwrap();
            let engine = data.join("engine");
            Self { _temp: temp, base, data, engine }
        }

        fn fingerprint() -> String {
            "b".repeat(64)
        }

        fn cwd(&self, id: &str) -> PathBuf {
            self.data.join("grok").join(id).join(Self::fingerprint()).join("workspace")
        }

        fn cache(&self, id: &str) -> PathBuf {
            let cwd = self.cwd(id);
            fs::create_dir_all(cwd.join(".git/objects")).unwrap();
            fs::write(cwd.join("private"), b"private cache").unwrap();
            cwd
        }

        fn sessions(&self) -> PathBuf {
            self.engine.join("home/.grok/sessions")
        }

        fn session(&self, name: &str, metadata: Option<&[u8]>) -> PathBuf {
            let directory = self.sessions().join(name);
            fs::create_dir_all(directory.join(SESSION)).unwrap();
            fs::write(directory.join(SESSION).join("history.json"), b"private history").unwrap();
            if let Some(metadata) = metadata {
                fs::write(directory.join(".cwd"), metadata).unwrap();
            }
            directory
        }

        fn short_session(&self, cwd: &Path) -> PathBuf {
            let name = encode(cwd.to_str().unwrap());
            assert!(name.len() <= 255, "test needs a short encoded cwd");
            self.session(&name, None)
        }

        fn long_session(&self, cwd: &Path, name: &str) -> PathBuf {
            assert!(encode(cwd.to_str().unwrap()).len() > 255, "test needs upstream's hash fallback");
            self.session(name, Some(cwd.as_os_str().as_bytes()))
        }

        fn retained(ids: &[&str]) -> HashSet<String> {
            ids.iter().map(|id| (*id).to_owned()).collect()
        }

        fn cleanup(&self, removed: &[&str], retained: &[&str], sweep: bool) -> io::Result<()> {
            cleanup(&self.data, &self.engine, &removed.iter().map(|id| (*id).to_owned()).collect::<Vec<_>>(), &Self::retained(retained), sweep)
        }
    }

    fn encode(value: &str) -> String {
        let mut encoded = String::new();
        for byte in value.bytes() {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                encoded.push(byte as char);
            } else {
                use std::fmt::Write;
                write!(encoded, "%{byte:02X}").unwrap();
            }
        }
        encoded
    }

    fn assert_private(path: &Path) {
        assert_eq!(fs::read(path).unwrap(), b"private cache");
    }

    #[test]
    fn removes_selected_cache_and_short_engine_cwd_but_protects_all_retained_conversations() {
        let f = Fixture::new(false);
        let removed = f.cache(REMOVED);
        let retained = f.cache(RETAINED);
        let parent = f.cache(PARENT);
        let orphan = f.cache(ORPHAN);
        let removed_session = f.short_session(&removed);
        let retained_session = f.short_session(&retained);
        let parent_session = f.short_session(&parent);
        let orphan_session = f.short_session(&orphan);
        let attachment = f.data.join("attachments").join(REMOVED).join("keep");
        fs::create_dir_all(attachment.parent().unwrap()).unwrap();
        fs::write(&attachment, b"attachment is not worker cleanup's responsibility").unwrap();

        f.cleanup(&[REMOVED, RETAINED, PARENT, REMOVED], &[RETAINED, PARENT], false).unwrap();

        assert!(!f.data.join("grok").join(REMOVED).exists());
        assert!(!removed_session.exists());
        for cwd in [&retained, &parent, &orphan] {
            assert_private(&cwd.join("private"));
        }
        for directory in [retained_session, parent_session, orphan_session] {
            assert!(directory.join(SESSION).join("history.json").is_file());
        }
        assert!(attachment.is_file());
        assert!(f.sessions().is_dir());
        // Retry after a successful cleanup is harmless.
        f.cleanup(&[REMOVED], &[RETAINED, PARENT], false).unwrap();
    }

    #[test]
    fn removes_long_cwd_metadata_sessions_without_reading_beyond_the_bound() {
        let f = Fixture::new(true);
        let removed = f.cache(REMOVED);
        let retained = f.cache(RETAINED);
        let deleted = f.long_session(&removed, HASH_NAME);
        let protected = f.long_session(&retained, "workspace-1123456789abcdef");
        let oversized = f.session("workspace-2123456789abcdef", Some(&vec![b'x'; MAX_CWD_BYTES + 1]));
        let invalid_utf8 = f.session("workspace-3123456789abcdef", Some(&[0xff, 0xfe]));
        let no_metadata = f.session("workspace-4123456789abcdef", None);
        // Upstream trims metadata; tolerate a trailing newline without normalizing cwd components.
        fs::write(deleted.join(".cwd"), format!("{}\n", removed.display())).unwrap();

        f.cleanup(&[REMOVED, RETAINED], &[RETAINED], false).unwrap();

        assert!(!deleted.exists());
        assert!(!f.data.join("grok").join(REMOVED).exists());
        assert_private(&retained.join("private"));
        for directory in [protected, oversized, invalid_utf8, no_metadata] {
            assert!(directory.join(SESSION).join("history.json").is_file());
        }
    }

    #[test]
    fn startup_sweeps_orphans_including_metadata_sessions_whose_private_cache_is_already_gone() {
        let f = Fixture::new(true);
        let orphan = f.cache(REMOVED);
        let retained = f.cache(RETAINED);
        let orphan_session = f.long_session(&orphan, HASH_NAME);
        let retained_session = f.long_session(&retained, "workspace-1123456789abcdef");
        let engine_only = f.long_session(&f.cwd(ORPHAN), "workspace-2123456789abcdef");
        assert!(!f.cwd(ORPHAN).exists());
        let invalid_cache = f.cache("not-a-uuid");

        f.cleanup(&[], &[RETAINED], true).unwrap();

        assert!(!orphan_session.exists());
        assert!(!engine_only.exists());
        assert!(!f.data.join("grok").join(REMOVED).exists());
        assert_private(&retained.join("private"));
        assert_private(&invalid_cache.join("private"));
        assert!(retained_session.join(SESSION).join("history.json").is_file());
    }

    #[test]
    fn startup_also_sweeps_short_encoded_orphan_sessions_without_a_grok_root() {
        let f = Fixture::new(false);
        let orphan = f.short_session(&f.cwd(REMOVED));
        let retained = f.short_session(&f.cwd(RETAINED));
        assert!(!f.data.join("grok").exists());

        f.cleanup(&[], &[RETAINED], true).unwrap();

        assert!(!orphan.exists());
        assert!(retained.join(SESSION).join("history.json").is_file());
    }

    #[test]
    fn invalid_ids_traversal_noncanonical_layouts_and_foreign_cwds_are_ignored() {
        let f = Fixture::new(false);
        let retained = f.cache(RETAINED);
        let simple_uuid = Uuid::parse_str(REMOVED).unwrap().simple().to_string();
        let invalid_cache = f.cache(&simple_uuid);
        let invalid_named_cache = f.cache("not-a-uuid");
        let outside = f.base.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("sentinel"), b"outside").unwrap();
        let valid_cwd = f.cwd(REMOVED).to_str().unwrap().to_owned();
        let mut directories = Vec::new();
        for cwd in [
            f.cwd(&simple_uuid).to_str().unwrap().to_owned(),
            f.cwd("not-a-uuid").to_str().unwrap().to_owned(),
            format!("{}/grok/{REMOVED}/{}/../workspace", f.data.display(), Fixture::fingerprint()),
            valid_cwd.replace("/workspace", "/./workspace"),
            valid_cwd.replace("/grok/", "//grok/"),
            format!("{}/grok/{REMOVED}/{}/workspace/extra", f.data.display(), Fixture::fingerprint()),
            format!("{}/grok/{REMOVED}/{}/workspace", f.data.display(), "B".repeat(64)),
            outside.to_str().unwrap().to_owned(),
        ] {
            let name = encode(&cwd);
            assert!(name.len() <= 255);
            directories.push(f.session(&name, None));
        }
        directories.push(f.session("%GG-malformed", None));
        directories.push(f.session("%FF", None));
        directories.push(f.session("workspace-not-a-hash", Some(valid_cwd.as_bytes())));
        directories.push(f.session("workspace-0123456789abcdef", Some(outside.as_os_str().as_bytes())));

        f.cleanup(&[&simple_uuid, "not-a-uuid", "../outside", "../../outside"], &[RETAINED], true).unwrap();

        assert_private(&retained.join("private"));
        assert_private(&invalid_cache.join("private"));
        assert_private(&invalid_named_cache.join("private"));
        for directory in directories {
            assert!(directory.join(SESSION).join("history.json").is_file());
        }
        assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"outside");
    }

    #[test]
    fn retained_uuid_identity_wins_even_with_a_noncanonical_retained_spelling() {
        let f = Fixture::new(false);
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let cwd = f.cache(id);
        let session = f.short_session(&cwd);
        let retained = HashSet::from([id.to_uppercase()]);

        cleanup(&f.data, &f.engine, &[id.to_owned()], &retained, true).unwrap();

        assert_private(&cwd.join("private"));
        assert!(session.join(SESSION).join("history.json").is_file());
    }

    #[test]
    fn symlinked_data_root_or_ancestor_is_rejected_before_any_deletion() {
        let f = Fixture::new(false);
        let cwd = f.cache(REMOVED);
        let session = f.short_session(&cwd);
        let alias = f.base.join("data-alias");
        symlink(&f.data, &alias).unwrap();
        let ancestor = f.base.join("ancestor-alias");
        symlink(&f.base, &ancestor).unwrap();

        for data in [&alias, &ancestor.join("d")] {
            assert!(cleanup(data, &f.engine, &[REMOVED.to_owned()], &HashSet::new(), true).is_err());
            assert_private(&cwd.join("private"));
            assert!(session.join(SESSION).join("history.json").is_file());
        }
    }

    #[test]
    fn symlinked_cache_root_and_engine_root_chain_never_scan_or_delete_outside() {
        for link in ["engine", "engine/home", "engine/home/.grok", "engine/home/.grok/sessions", "grok"] {
            let f = Fixture::new(false);
            let retained = if link == "grok" { None } else { Some(f.cache(RETAINED)) };
            let outside = f.base.join("outside");
            fs::create_dir_all(outside.join(REMOVED)).unwrap();
            fs::write(outside.join(REMOVED).join("sentinel"), b"outside").unwrap();
            let target = f.data.join(link);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            symlink(&outside, &target).unwrap();

            assert!(f.cleanup(&[REMOVED, RETAINED], &[RETAINED], true).is_err(), "{link}");

            assert_eq!(fs::read(outside.join(REMOVED).join("sentinel")).unwrap(), b"outside", "{link}");
            assert!(fs::symlink_metadata(&target).unwrap().file_type().is_symlink());
            if let Some(retained) = retained {
                assert_private(&retained.join("private"));
            }
        }
    }

    #[test]
    fn candidate_and_cwd_ancestor_symlinks_are_not_followed() {
        let f = Fixture::new(false);
        let retained = f.cache(RETAINED);
        let outside = f.base.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("sentinel"), b"outside").unwrap();
        fs::create_dir_all(f.data.join("grok")).unwrap();
        symlink(&outside, f.data.join("grok").join(REMOVED)).unwrap();
        let matching_session = f.short_session(&f.cwd(REMOVED));
        let linked_engine_entry = f.sessions().join(encode(f.cwd(ORPHAN).to_str().unwrap()));
        symlink(&outside, &linked_engine_entry).unwrap();
        // A fingerprint symlink is also insufficient to establish ownership of its engine cwd.
        fs::create_dir_all(f.data.join("grok").join(ORPHAN)).unwrap();
        symlink(&outside, f.data.join("grok").join(ORPHAN).join(Fixture::fingerprint())).unwrap();
        let orphan_metadata = f.session(HASH_NAME, Some(f.cwd(ORPHAN).as_os_str().as_bytes()));

        f.cleanup(&[REMOVED, ORPHAN], &[RETAINED], true).unwrap();

        assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"outside");
        assert!(fs::symlink_metadata(f.data.join("grok").join(REMOVED)).unwrap().file_type().is_symlink());
        assert!(matching_session.join(SESSION).join("history.json").is_file());
        assert!(fs::symlink_metadata(linked_engine_entry).unwrap().file_type().is_symlink());
        assert!(orphan_metadata.join(SESSION).join("history.json").is_file());
        assert_private(&retained.join("private"));
    }

    #[test]
    fn cwd_metadata_symlink_is_not_read_and_interior_links_are_only_unlinked() {
        let f = Fixture::new(false);
        let removed = f.cache(REMOVED);
        let retained = f.cache(RETAINED);
        let outside = f.base.join("outside");
        fs::create_dir_all(&outside).unwrap();
        let metadata = outside.join("cwd");
        fs::write(&metadata, removed.as_os_str().as_bytes()).unwrap();
        fs::write(outside.join("sentinel"), b"outside").unwrap();
        symlink(&outside, removed.join("outside-link")).unwrap();
        let deleted_session = f.short_session(&removed);
        symlink(&outside, deleted_session.join("outside-link")).unwrap();
        let untrusted_metadata = f.session(HASH_NAME, None);
        symlink(&metadata, untrusted_metadata.join(".cwd")).unwrap();

        f.cleanup(&[REMOVED], &[RETAINED], false).unwrap();

        assert!(!f.data.join("grok").join(REMOVED).exists());
        assert!(!deleted_session.exists());
        assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"outside");
        assert_eq!(fs::read(metadata).unwrap(), removed.as_os_str().as_bytes());
        assert!(untrusted_metadata.join(SESSION).join("history.json").is_file());
        assert!(fs::symlink_metadata(untrusted_metadata.join(".cwd")).unwrap().file_type().is_symlink());
        assert_private(&retained.join("private"));
    }

    #[test]
    fn filesystem_failure_returns_error_but_still_tries_safe_cleanup_without_touching_retained() {
        let f = Fixture::new(false);
        f.cache(REMOVED);
        let retained = f.cache(RETAINED);
        let sessions = f.sessions();
        fs::create_dir_all(sessions.parent().unwrap()).unwrap();
        fs::write(&sessions, b"not a directory").unwrap();

        assert!(f.cleanup(&[REMOVED, RETAINED], &[RETAINED], false).is_err());

        assert!(!f.data.join("grok").join(REMOVED).exists());
        assert_private(&retained.join("private"));
        assert_eq!(fs::read(sessions).unwrap(), b"not a directory");
    }

    #[test]
    fn empty_or_invalid_selection_does_not_touch_filesystem_and_root_traversal_is_rejected() {
        let f = Fixture::new(false);
        let cwd = f.cache(RETAINED);
        f.cleanup(&[], &[RETAINED], false).unwrap();
        f.cleanup(&["../outside", "not-a-uuid"], &[RETAINED], false).unwrap();
        f.cleanup(&[RETAINED], &[RETAINED], false).unwrap();
        assert!(cleanup(&f.data.join(".."), &f.engine, &[REMOVED.to_owned()], &HashSet::new(), false).is_err());
        assert_private(&cwd.join("private"));
        assert_eq!(percent_decode("%2fapp%2Bprivate"), Some("/app+private".into()));
        for invalid in ["%", "%2", "%GG", "%FF"] {
            assert!(percent_decode(invalid).is_none());
        }
    }
}
