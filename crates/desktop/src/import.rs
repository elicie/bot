//! One-time, offline import from an existing web data directory.
//!
//! The caller must stop the source web server, resolve both paths before the engine changes HOME,
//! and call this on its startup runtime before opening the destination Backend. The destination
//! must not exist (even an empty directory is refused). Engine configuration/sessions, backups,
//! logs and browser preferences are deliberately not imported. The original is never opened by
//! Store, chmodded, renamed or written; only a disposable copy is passed to the real Store loader.

use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use grok_bot_store::{MAX_STATE_BYTES, Store};
use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, RenameFlags};
use rustix::io::Errno;
use serde_json::Value;

const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY.union(OFlags::DIRECTORY).union(OFlags::NOFOLLOW).union(OFlags::CLOEXEC);
const READ_FLAGS: OFlags = OFlags::RDONLY.union(OFlags::NOFOLLOW).union(OFlags::NONBLOCK).union(OFlags::CLOEXEC);
const CREATE_FLAGS: OFlags = OFlags::WRONLY.union(OFlags::CREATE).union(OFlags::EXCL).union(OFlags::NOFOLLOW).union(OFlags::CLOEXEC);
const ASSET_DIRECTORIES: [&str; 3] = ["attachments", "generated-images", "knowledge"];
// Explicit limits make importing an untrusted or accidentally huge directory bounded. Larger
// installations need an explicit future migration path rather than a silently truncated import.
const MAX_ASSET_FILE_BYTES: u64 = 20 * 1024 * 1024;
const MAX_IMPORT_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = 100_000;
const MAX_DEPTH: usize = 8;

/// Copy a stopped web installation into a NEW desktop store directory, atomically and privately.
/// Uses the caller's already-prepared runtime; it never changes the process environment or starts
/// an engine. All parsing/loader errors are deliberately generic so credentials cannot enter logs.
/// Blocking file copying is startup-only, before Backend admission or the HTTP listener starts.
pub async fn import_web_data(source: &Path, destination: &Path) -> Result<()> {
    let prepared = Prepared::copy(source, destination)?;
    // Store::open repairs and rewrites state and prunes knowledge. Validate a separate state-only
    // copy, not the source or the final staged tree, so none of those changes become import data.
    let validation = tempfile::Builder::new().prefix(".validate-").tempdir_in(prepared.stage.path()).context("Cannot stage web data validation.")?;
    write_file(&open_directory(validation.path(), false)?, OsStr::new("state.json"), &prepared.original)?;
    let checked = Store::open(validation.path(), grok_bot_model::system_clock()).await.map_err(|_| anyhow!("Web data is not valid application state."))?;
    checked.flush().await.map_err(|_| anyhow!("Cannot validate the copied web data."))?;
    drop(checked);
    validation.close().context("Cannot remove the disposable web data validation copy.")?;
    prepared.publish()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    device: u64,
    inode: u64,
    mode: u32,
    links: u64,
    bytes: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl Stamp {
    fn of(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.mode(),
            links: metadata.nlink(),
            bytes: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

struct Record {
    relative: PathBuf,
    stamp: Stamp,
    directory: bool,
}

struct Budget {
    bytes: u64,
    entries: usize,
}

impl Budget {
    fn reserve(&mut self, bytes: u64) -> Result<()> {
        self.entries += 1;
        self.bytes = self.bytes.checked_add(bytes).ok_or_else(|| anyhow!("Web data exceeds the import size limit."))?;
        ensure!(self.entries <= MAX_ENTRIES && self.bytes <= MAX_IMPORT_BYTES, "Web data exceeds the import size or entry limit.");
        Ok(())
    }
}

struct Prepared {
    source_path: PathBuf,
    parent_path: PathBuf,
    parent: File,
    name: OsString,
    stage: tempfile::TempDir,
    stage_directory: File,
    original: Vec<u8>,
    records: Vec<Record>,
}

impl Prepared {
    fn copy(source: &Path, destination: &Path) -> Result<Self> {
        let source_path = absolute_path(source)?;
        let destination = absolute_path(destination)?;
        ensure!(!destination.starts_with(&source_path), "Import destination must not be inside the source web data.");
        let parent_path = destination.parent().ok_or_else(|| anyhow!("Import destination needs a parent directory."))?.to_path_buf();
        let name = destination.file_name().ok_or_else(|| anyhow!("Import destination needs its own directory."))?.to_owned();
        let source = open_directory(&source_path, false)?;
        let source_stamp = Stamp::of(&source.metadata()?);
        let (original, state_stamp) = read_state(&source)?;
        // Typed parsing rejects unknown/duplicate fields and malformed session structures before
        // removing session pointers. Value rewriting preserves every other persistent field.
        serde_json::from_slice::<grok_bot_model::DiskState>(&original).map_err(|_| anyhow!("Web data is not valid application state."))?;
        let rewritten = clear_sessions(&original)?;
        let parent = open_directory(&parent_path, true)?;
        ensure_absent(&parent, &name)?;
        let stage = tempfile::Builder::new().prefix(".grok-bot-import-").tempdir_in(&parent_path).context("Cannot stage private web data.")?;
        let stage_directory = open_directory(stage.path(), false)?;
        rustix::fs::fchmod(&stage_directory, Mode::RWXU)?;
        let mut prepared = Self {
            source_path,
            parent_path,
            parent,
            name,
            stage,
            stage_directory,
            original,
            records: vec![
                Record { relative: PathBuf::new(), stamp: source_stamp, directory: true },
                Record { relative: PathBuf::from("state.json"), stamp: state_stamp, directory: false },
            ],
        };
        let mut budget = Budget { bytes: prepared.original.len() as u64, entries: 1 };
        write_file(&prepared.stage_directory, OsStr::new("state.json"), &rewritten)?;
        for directory in ASSET_DIRECTORIES {
            let child = match rustix::fs::openat(&source, directory, DIRECTORY_FLAGS, Mode::empty()) {
                Ok(child) => File::from(child),
                Err(Errno::NOENT) => continue,
                Err(_) => bail!("Web asset directories must be real directories, not symbolic links."),
            };
            budget.reserve(0)?;
            let output = create_directory(&prepared.stage_directory, OsStr::new(directory))?;
            copy_tree(&child, &output, Path::new(directory), &mut budget, &mut prepared.records, 1)?;
        }
        rustix::fs::fsync(&prepared.stage_directory)?;
        Ok(prepared)
    }

    fn publish(self) -> Result<()> {
        // Both the source state bytes and every copied inode's metadata are rechecked after the
        // asynchronous Store validation. This detects a still-running server or an edited source;
        // it is not a live snapshot protocol, hence the explicit stopped-source prerequisite.
        let source = open_directory(&self.source_path, false)?;
        for record in &self.records {
            let file = open_relative(&source, &record.relative, record.directory)?;
            ensure!(Stamp::of(&file.metadata()?) == record.stamp, "Source web data changed during import. Stop the web server and retry.");
        }
        let (current, _) = read_state(&source)?;
        ensure!(current == self.original, "Source state changed during import. Stop the web server and retry.");
        let current_parent = open_directory(&self.parent_path, false)?;
        ensure!(same_inode(&current_parent, &self.parent)?, "Import destination parent changed during import.");
        let stage_name = self.stage.path().file_name().ok_or_else(|| anyhow!("Invalid import staging directory."))?;
        let current_stage = File::from(rustix::fs::openat(&self.parent, stage_name, DIRECTORY_FLAGS, Mode::empty())?);
        ensure!(same_inode(&current_stage, &self.stage_directory)?, "Import staging directory changed during import.");
        ensure_absent(&self.parent, &self.name)?;
        rustix::fs::fsync(&self.stage_directory)?;
        // NOREPLACE is renameat2(RENAME_NOREPLACE) on Linux and renameatx_np(RENAME_EXCL) on
        // macOS: even a raced destination is never replaced. No unsafe fallback is attempted.
        rustix::fs::renameat_with(&self.parent, stage_name, &self.parent, &self.name, RenameFlags::NOREPLACE)
            .context("Cannot publish imported data without replacing an existing destination.")?;
        rustix::fs::fsync(&self.parent).context("Imported data was published, but syncing its parent directory failed.")?;
        Ok(())
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    let path = std::path::absolute(path).context("Cannot resolve an import path.")?;
    ensure!(!path.components().any(|part| matches!(part, Component::ParentDir | Component::Prefix(_))), "Import paths must not contain parent traversal.");
    Ok(path)
}

/// Walk every ancestor without following links. Existing parent directories keep their modes;
/// only newly created import parents are 0700. Resolve macOS /var aliases before calling import.
fn open_directory(path: &Path, create: bool) -> Result<File> {
    let mut directory = File::from(rustix::fs::open("/", DIRECTORY_FLAGS, Mode::empty())?);
    for part in path.components() {
        let Component::Normal(name) = part else { continue };
        let child = match rustix::fs::openat(&directory, name, DIRECTORY_FLAGS, Mode::empty()) {
            Ok(child) => child,
            Err(Errno::NOENT) if create => {
                match rustix::fs::mkdirat(&directory, name, Mode::RWXU) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(error) => return Err(error.into()),
                }
                rustix::fs::openat(&directory, name, DIRECTORY_FLAGS, Mode::empty())?
            }
            Err(_) => bail!("Import paths must be accessible real directories without symbolic links."),
        };
        directory = File::from(child);
    }
    Ok(directory)
}

fn ensure_absent(parent: &File, name: &OsStr) -> Result<()> {
    match rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Err(Errno::NOENT) => Ok(()),
        Ok(_) => bail!("Desktop data already exists. Import never merges or overwrites a destination."),
        Err(error) => Err(error.into()),
    }
}

fn same_inode(left: &File, right: &File) -> Result<bool> {
    let (left, right) = (left.metadata()?, right.metadata()?);
    Ok(left.dev() == right.dev() && left.ino() == right.ino())
}

fn read_state(source: &File) -> Result<(Vec<u8>, Stamp)> {
    let file = File::from(rustix::fs::openat(source, "state.json", READ_FLAGS, Mode::empty()).context("Cannot read source state.json.")?);
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.nlink() == 1 && metadata.len() <= MAX_STATE_BYTES as u64,
        "Source state must be a regular single-link file of at most 64 MiB."
    );
    let stamp = Stamp::of(&metadata);
    let mut bytes = Vec::new();
    (&file).take(MAX_STATE_BYTES as u64 + 1).read_to_end(&mut bytes).context("Cannot read source state.")?;
    ensure!(bytes.len() as u64 == metadata.len() && Stamp::of(&file.metadata()?) == stamp, "Source state changed while being read.");
    Ok((bytes, stamp))
}

fn clear_sessions(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut state: Value = serde_json::from_slice(bytes).map_err(|_| anyhow!("Web data is not valid application state."))?;
    let conversations = state.get_mut("conversations").and_then(Value::as_array_mut).ok_or_else(|| anyhow!("Web data is not valid application state."))?;
    for conversation in conversations {
        let conversation = conversation.as_object_mut().ok_or_else(|| anyhow!("Web data is not valid application state."))?;
        for field in ["sessionId", "sessionFingerprint", "memberSessions"] {
            conversation.remove(field);
        }
    }
    let bytes = serde_json::to_vec(&state).context("Cannot encode imported state.")?;
    ensure!(bytes.len() <= MAX_STATE_BYTES, "Imported state exceeds the 64 MiB limit.");
    Ok(bytes)
}

fn create_directory(parent: &File, name: &OsStr) -> Result<File> {
    rustix::fs::mkdirat(parent, name, Mode::RWXU)?;
    let child = File::from(rustix::fs::openat(parent, name, DIRECTORY_FLAGS, Mode::empty())?);
    rustix::fs::fchmod(&child, Mode::RWXU)?;
    Ok(child)
}

fn write_file(parent: &File, name: &OsStr, bytes: &[u8]) -> Result<()> {
    let mut output = File::from(rustix::fs::openat(parent, name, CREATE_FLAGS, Mode::RUSR | Mode::WUSR)?);
    rustix::fs::fchmod(&output, Mode::RUSR | Mode::WUSR)?;
    output.write_all(bytes)?;
    output.sync_all()?;
    Ok(())
}

fn copy_tree(source: &File, output: &File, relative: &Path, budget: &mut Budget, records: &mut Vec<Record>, depth: usize) -> Result<()> {
    ensure!(depth <= MAX_DEPTH, "Web assets exceed the import directory depth limit.");
    records.push(Record { relative: relative.to_path_buf(), stamp: Stamp::of(&source.metadata()?), directory: true });
    for entry in Dir::read_from(source)? {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        let name = OsStr::from_bytes(name);
        let child_path = relative.join(name);
        let stat = rustix::fs::statat(source, name, AtFlags::SYMLINK_NOFOLLOW)?;
        match FileType::from_raw_mode(stat.st_mode as _) {
            FileType::Directory => {
                budget.reserve(0)?;
                let child = File::from(rustix::fs::openat(source, name, DIRECTORY_FLAGS, Mode::empty())?);
                let destination = create_directory(output, name)?;
                copy_tree(&child, &destination, &child_path, budget, records, depth + 1)?;
            }
            FileType::RegularFile => {
                let file = File::from(rustix::fs::openat(source, name, READ_FLAGS, Mode::empty())?);
                let metadata = file.metadata()?;
                ensure!(
                    metadata.is_file() && metadata.nlink() == 1 && metadata.len() <= MAX_ASSET_FILE_BYTES,
                    "Web assets must be regular single-link files of at most 20 MiB."
                );
                budget.reserve(metadata.len())?;
                let stamp = Stamp::of(&metadata);
                let mut destination = File::from(rustix::fs::openat(output, name, CREATE_FLAGS, Mode::RUSR | Mode::WUSR)?);
                rustix::fs::fchmod(&destination, Mode::RUSR | Mode::WUSR)?;
                let copied = std::io::copy(&mut (&file).take(MAX_ASSET_FILE_BYTES + 1), &mut destination)?;
                ensure!(copied == metadata.len() && Stamp::of(&file.metadata()?) == stamp, "Source web assets changed during import.");
                destination.sync_all()?;
                records.push(Record { relative: child_path, stamp, directory: false });
            }
            _ => bail!("Web assets must not contain symbolic links or special files."),
        }
    }
    rustix::fs::fsync(output)?;
    Ok(())
}

fn open_relative(root: &File, path: &Path, directory: bool) -> Result<File> {
    let mut parent = root.try_clone()?;
    let mut parts = path.components().peekable();
    while let Some(part) = parts.next() {
        let Component::Normal(name) = part else { bail!("Invalid import asset path.") };
        let flags = if parts.peek().is_some() || directory { DIRECTORY_FLAGS } else { READ_FLAGS };
        parent = File::from(rustix::fs::openat(&parent, name, flags, Mode::empty()).context("Source web data changed during import.")?);
    }
    Ok(parent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

    use serde_json::json;

    const BOT: &str = "370d0fed-454e-4155-b785-1f122c73d1a1";
    const CONVERSATION: &str = "d2b4de70-ffe1-488a-9e0e-af105c1e00a9";
    const ATTACHMENT: &str = "a296570d-785d-4fc2-8a8e-5f393d813cbb";
    const IMAGE: &str = "b905e27f-1882-4480-88c8-1545abf1a30b";
    const KNOWLEDGE: &str = "f802b94c-5f7c-45a0-8646-54f051cc25e4";
    const NOW: &str = "2026-01-01T00:00:00.000Z";

    struct Fixture {
        _temporary: tempfile::TempDir,
        root: PathBuf,
        source: PathBuf,
        destination: PathBuf,
    }

    fn state() -> Value {
        json!({
            "version": 1, "revision": 42,
            "settings": { "baseUrl": "http://127.0.0.1:12345/v1", "model": "fixture", "apiKey": "private-fixture-key", "workspace": "" },
            "bots": [{ "id": BOT, "name": "Grok", "color": "#8b5cf6", "instructions": "Preserved instructions", "createdAt": NOW }],
            "conversations": [{ "id": CONVERSATION, "botId": BOT, "title": "History", "messages": [{
                "id": "b1d54d29-0f1f-47c4-ab65-bdb403013df8", "role": "user", "text": "Preserved transcript", "createdAt": NOW, "tools": []
            }], "run": { "id": "", "status": "idle" }, "updatedAt": NOW,
            "sessionId": "old-session", "sessionFingerprint": "old-fingerprint",
            "memberSessions": [{ "botId": BOT, "sessionId": "old-member-session", "sessionFingerprint": "old-member-fingerprint" }] }],
            "routines": [], "routineRuns": [], "botExchanges": [], "botDeliveries": [],
            "permissionRules": [], "groups": [], "secrets": [], "mcpServers": [], "mainBotId": BOT,
        })
    }

    fn fixture() -> Fixture {
        let temporary = tempfile::tempdir().unwrap();
        // macOS temp directories may have /var aliases. The importer intentionally refuses link
        // ancestors, just like the existing files crate; use the canonical directory in tests.
        let root = fs::canonicalize(temporary.path()).unwrap();
        let source = root.join("web");
        let destination = root.join("desktop").join("store");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("state.json"), serde_json::to_vec(&state()).unwrap()).unwrap();
        Fixture { _temporary: temporary, root, source, destination }
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    fn no_staging(f: &Fixture) {
        let parent = f.destination.parent().unwrap();
        if parent.exists() {
            assert!(fs::read_dir(parent).unwrap().all(|entry| !entry.unwrap().file_name().to_string_lossy().starts_with(".grok-bot-import-")));
        }
    }

    #[tokio::test]
    async fn preserves_original_and_all_persistent_fields_except_engine_sessions() {
        let f = fixture();
        fs::set_permissions(f.source.join("state.json"), fs::Permissions::from_mode(0o640)).unwrap();
        let before = fs::read(f.source.join("state.json")).unwrap();
        import_web_data(&f.source, &f.destination).await.unwrap();
        assert_eq!(fs::read(f.source.join("state.json")).unwrap(), before);
        assert_eq!(fs::metadata(f.source.join("state.json")).unwrap().permissions().mode() & 0o777, 0o640);
        let mut expected = state();
        let conversation = expected["conversations"][0].as_object_mut().unwrap();
        for field in ["sessionId", "sessionFingerprint", "memberSessions"] {
            conversation.remove(field);
        }
        let imported = read_json(&f.destination.join("state.json"));
        assert_eq!(imported, expected);
        assert_eq!(imported["revision"], 42, "the validator must not publish Store's rewritten copy");
        assert_eq!(fs::metadata(&f.destination).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(f.destination.join("state.json")).unwrap().permissions().mode() & 0o777, 0o600);
        no_staging(&f);
    }

    #[tokio::test]
    async fn refuses_existing_destination_without_merging_or_overwriting() {
        let f = fixture();
        fs::create_dir_all(&f.destination).unwrap();
        fs::write(f.destination.join("marker"), b"existing desktop data").unwrap();
        assert!(import_web_data(&f.source, &f.destination).await.is_err());
        assert_eq!(fs::read(f.destination.join("marker")).unwrap(), b"existing desktop data");
        assert!(!f.destination.join("state.json").exists());
        no_staging(&f);
    }

    #[tokio::test]
    async fn rejects_malformed_and_semantically_invalid_state_without_partial_destination() {
        for invalid in [b"not JSON".to_vec(), serde_json::to_vec(&json!({ "version": 1 })).unwrap(), {
            let mut bad = state();
            bad["conversations"][0]["botId"] = json!("11111111-1111-4111-8111-111111111111");
            serde_json::to_vec(&bad).unwrap()
        }] {
            let f = fixture();
            fs::write(f.source.join("state.json"), &invalid).unwrap();
            let error = import_web_data(&f.source, &f.destination).await.unwrap_err();
            assert!(!error.to_string().contains("private-fixture-key"));
            assert!(!f.destination.exists());
            assert_eq!(fs::read(f.source.join("state.json")).unwrap(), invalid);
            no_staging(&f);
        }
    }

    #[tokio::test]
    async fn copies_all_asset_trees_with_private_modes_but_not_engine_or_transients() {
        let f = fixture();
        let assets = [
            (format!("attachments/{CONVERSATION}/{ATTACHMENT}"), b"attachment bytes".as_slice()),
            (format!("generated-images/{IMAGE}"), b"\x89PNG\r\n\x1a\nimage bytes".as_slice()),
            (format!("knowledge/{BOT}/{KNOWLEDGE}.txt"), b"knowledge bytes".as_slice()),
        ];
        for (relative, bytes) in &assets {
            let path = f.source.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
        fs::create_dir_all(f.source.join("engine/home/.grok")).unwrap();
        fs::write(f.source.join("engine/home/.grok/config.toml"), b"do not import").unwrap();
        fs::write(f.source.join("server.log"), b"do not import").unwrap();
        import_web_data(&f.source, &f.destination).await.unwrap();
        for (relative, bytes) in assets {
            let path = f.destination.join(relative);
            assert_eq!(fs::read(&path).unwrap(), bytes);
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        }
        assert!(!f.destination.join("engine").exists());
        assert!(!f.destination.join("server.log").exists());
        no_staging(&f);
    }

    #[tokio::test]
    async fn rejects_symlinks_in_source_state_asset_roots_and_nested_assets() {
        for placement in ["state", "asset-root", "nested"] {
            let f = fixture();
            let outside = f.root.join("outside");
            fs::write(&outside, b"outside stays unchanged").unwrap();
            match placement {
                "state" => {
                    let state_path = f.source.join("state.json");
                    let saved = f.root.join("real-state.json");
                    fs::rename(&state_path, &saved).unwrap();
                    symlink(saved, state_path).unwrap();
                }
                "asset-root" => {
                    symlink(&f.root, f.source.join("attachments")).unwrap();
                }
                _ => {
                    fs::create_dir(f.source.join("attachments")).unwrap();
                    symlink(&outside, f.source.join("attachments/linked")).unwrap();
                }
            }
            assert!(import_web_data(&f.source, &f.destination).await.is_err(), "{placement}");
            assert!(!f.destination.exists());
            assert_eq!(fs::read(outside).unwrap(), b"outside stays unchanged");
            no_staging(&f);
        }
    }

    #[tokio::test]
    async fn rejects_hardlinks_special_files_and_oversized_assets() {
        for kind in ["hardlink", "fifo", "oversized"] {
            let f = fixture();
            fs::create_dir(f.source.join("attachments")).unwrap();
            let asset = f.source.join("attachments/asset");
            match kind {
                "hardlink" => {
                    fs::write(f.root.join("outside"), b"not a unique file").unwrap();
                    fs::hard_link(f.root.join("outside"), &asset).unwrap();
                }
                "fifo" => {
                    let name = std::ffi::CString::new(asset.as_os_str().as_bytes()).unwrap();
                    // SAFETY: a valid, NUL-terminated temporary fixture path; POSIX mkfifo is
                    // available on both supported OSes (rustix mknodat is not exposed on macOS).
                    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
                }
                _ => File::create(&asset).unwrap().set_len(MAX_ASSET_FILE_BYTES + 1).unwrap(),
            }
            assert!(import_web_data(&f.source, &f.destination).await.is_err(), "{kind}");
            assert!(!f.destination.exists());
            no_staging(&f);
        }
    }

    #[test]
    fn refuses_changed_source_and_raced_destination_at_atomic_publish() {
        for race in ["source", "destination"] {
            let f = fixture();
            let prepared = Prepared::copy(&f.source, &f.destination).unwrap();
            match race {
                "source" => fs::write(f.source.join("state.json"), b"changed after copy").unwrap(),
                _ => fs::create_dir(&f.destination).unwrap(),
            }
            assert!(prepared.publish().is_err());
            assert!(!f.destination.join("state.json").exists());
            if race == "source" {
                assert!(!f.destination.exists());
            }
            no_staging(&f);
        }
    }
}
