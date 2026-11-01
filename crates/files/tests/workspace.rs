//! Ports of the `WorkspaceFiles` / files.ts cases in `tests/backend-api.test.ts` plus direct
//! coverage of the private-file helpers.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use grok_bot_files::{
    MAX_FILE_BYTES, WorkspaceFiles, atomic_private_file, canonical_directory, content_hash, io_error, private_directory,
};
use grok_bot_model::{FileEntryType, HttpError};

const LINK_ERROR: &str = "Symbolic links and non-directory parents are not allowed.";
const CONFLICT: &str = "File changed on disk. Reload it before saving.";

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
}

fn fixture() -> Fixture {
    let temp = tempfile::Builder::new().prefix("grok-bot-files-").tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let workspace = root.join("workspace");
    fs::create_dir(&workspace).unwrap();
    fs::write(workspace.join("notes.txt"), "original\n").unwrap();
    Fixture { _temp: temp, root, workspace }
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

fn names(path: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(path).unwrap().map(|entry| entry.unwrap().file_name().into_string().unwrap()).collect();
    names.sort();
    names
}

#[test]
fn workspace_browse_read_save_observes_hashes_and_rejects_traversal_links_binary_and_oversized_files() {
    let f = fixture();
    fs::create_dir(f.workspace.join("nested")).unwrap();
    fs::write(f.root.join("outside.txt"), "outside stays unchanged\n").unwrap();
    symlink(f.root.join("outside.txt"), f.workspace.join("linked.txt")).unwrap();
    symlink(&f.root, f.workspace.join("parent-link")).unwrap();
    fs::hard_link(f.root.join("outside.txt"), f.workspace.join("hard.txt")).unwrap();
    fs::write(f.workspace.join("binary.bin"), [0u8, 1, 2]).unwrap();
    fs::write(f.workspace.join("large.txt"), "x".repeat(MAX_FILE_BYTES + 1)).unwrap();
    let files = WorkspaceFiles::new(&f.workspace);

    let listing = files.list("").unwrap();
    assert_eq!(listing.path, "");
    assert_eq!(listing.entries[0].name, "nested");
    assert_eq!(listing.entries[0].entry_type, FileEntryType::Directory);
    assert!(!listing.entries.iter().any(|entry| entry.name == "linked.txt" || entry.name == "parent-link"));
    let listed: Vec<_> = listing.entries.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(listed, ["nested", "binary.bin", "hard.txt", "large.txt", "notes.txt"]);

    let document = files.read("notes.txt").unwrap();
    assert_eq!(document.content, "original\n");
    assert_eq!(document.path, "notes.txt");
    assert_eq!(document.hash.len(), 64);
    assert!(document.hash.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));
    assert_eq!(files.expected("notes.txt").unwrap(), document.hash);

    let saved = files.save("notes.txt", "saved from browser\n", &document.hash).unwrap();
    assert_eq!(saved.hash, content_hash(b"saved from browser\n"));
    assert_eq!(saved.content, "saved from browser\n");
    assert_eq!(fs::read_to_string(f.workspace.join("notes.txt")).unwrap(), "saved from browser\n");
    assert_eq!(files.save("notes.txt", "stale write", &document.hash).unwrap_err(), HttpError::conflict(CONFLICT));

    let bad_paths = [
        ("../outside.txt", "Path traversal is not allowed."),
        ("/etc/passwd", "Use a workspace-relative path."),
        ("nested/../../outside.txt", "Path traversal is not allowed."),
        ("parent-link/outside.txt", LINK_ERROR),
        ("linked.txt", "Symbolic links are not allowed."),
        ("hard.txt", "Hard-linked files are not allowed."),
        ("nested/../notes.txt", "Path traversal is not allowed."),
    ];
    for (filename, message) in bad_paths {
        assert_eq!(files.read(filename).unwrap_err(), HttpError::forbidden(message), "{filename}");
        assert_eq!(files.save(filename, "blocked", "").unwrap_err(), HttpError::forbidden(message), "{filename}");
    }
    assert_eq!(files.read("binary.bin").unwrap_err(), HttpError::new(415, "Binary files cannot be edited."));
    assert_eq!(files.read("large.txt").unwrap_err(), HttpError::too_large("Text files are limited to 1 MiB."));
    assert_eq!(files.read("missing.txt").unwrap_err(), HttpError::not_found("File or directory does not exist."));
    assert_eq!(files.expected("missing.txt").unwrap(), "");
    let created = files.save("new.txt", "new file\n", "").unwrap();
    assert_eq!(created.path, "new.txt");
    assert_eq!(fs::read_to_string(f.workspace.join("new.txt")).unwrap(), "new file\n");
    assert_eq!(fs::read_to_string(f.root.join("outside.txt")).unwrap(), "outside stays unchanged\n");
    assert!(fs::symlink_metadata(f.workspace.join("linked.txt")).unwrap().file_type().is_symlink());

    let alias = f.root.join("workspace-alias");
    symlink(&f.workspace, &alias).unwrap();
    assert_eq!(canonical_directory(alias.to_str().unwrap()).unwrap_err(), HttpError::forbidden(LINK_ERROR));
}

#[test]
fn relative_paths_are_validated_like_the_node_server() {
    let files = WorkspaceFiles::new("/nonexistent-workspace");
    assert_eq!(files.relative("", true).unwrap(), "");
    assert_eq!(files.relative(".", true).unwrap(), "");
    assert_eq!(files.relative("a/b.txt", false).unwrap(), "a/b.txt");
    assert_eq!(files.relative(".hidden", false).unwrap(), ".hidden");
    for input in ["/abs", "a\\b", "a\0b"] {
        assert_eq!(files.relative(input, true).unwrap_err(), HttpError::forbidden("Use a workspace-relative path."), "{input:?}");
    }
    for input in ["", ".", "..", "a//b", "a/", "./a", "a/./b", "a/../b", "../a"] {
        assert_eq!(files.relative(input, false).unwrap_err(), HttpError::forbidden("Path traversal is not allowed."), "{input:?}");
    }
    assert_eq!(files.list("../x").unwrap_err().status, 403);
    assert_eq!(files.read("").unwrap_err(), HttpError::forbidden("Path traversal is not allowed."));
}

#[test]
fn listing_handles_subdirectories_missing_paths_files_links_and_entry_limits() {
    let f = fixture();
    let files = WorkspaceFiles::new(&f.workspace);
    fs::create_dir_all(f.workspace.join("nested/deeper")).unwrap();
    for name in ["b.txt", "A.txt", "a.txt", "_x.txt", "10.txt", "9.txt"] {
        fs::write(f.workspace.join("nested").join(name), "x").unwrap();
    }
    let listing = files.list("nested").unwrap();
    assert_eq!(listing.path, "nested");
    let listed: Vec<_> = listing.entries.iter().map(|entry| (entry.name.as_str(), entry.path.as_str())).collect();
    assert_eq!(
        listed,
        [
            ("deeper", "nested/deeper"),
            ("_x.txt", "nested/_x.txt"),
            ("10.txt", "nested/10.txt"),
            ("9.txt", "nested/9.txt"),
            ("a.txt", "nested/a.txt"),
            ("A.txt", "nested/A.txt"),
            ("b.txt", "nested/b.txt"),
        ]
    );
    assert_eq!(files.list(".").unwrap().path, "");
    assert_eq!(files.list("missing").unwrap_err(), HttpError::not_found("File or directory does not exist."));
    assert_eq!(files.list("notes.txt").unwrap_err(), HttpError::forbidden(LINK_ERROR));
    symlink(f.workspace.join("nested"), f.workspace.join("dir-link")).unwrap();
    assert_eq!(files.list("dir-link").unwrap_err(), HttpError::forbidden(LINK_ERROR));

    let crowded = f.workspace.join("crowded");
    fs::create_dir(&crowded).unwrap();
    for index in 0..5000 {
        fs::write(crowded.join(index.to_string()), "").unwrap();
    }
    assert_eq!(files.list("crowded").unwrap().entries.len(), 5000);
    fs::create_dir(crowded.join("one-more")).unwrap();
    assert_eq!(files.list("crowded").unwrap_err(), HttpError::too_large("This directory has more than 5,000 entries."));
}

#[test]
fn reads_reject_non_regular_files_invalid_text_and_accept_limits() {
    let f = fixture();
    let files = WorkspaceFiles::new(&f.workspace);
    fs::create_dir(f.workspace.join("nested")).unwrap();
    assert_eq!(files.read("nested").unwrap_err(), HttpError::new(415, "Only regular text files can be edited."));
    rustix::fs::mknodat(
        rustix::fs::CWD,
        f.workspace.join("pipe"),
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        0,
    )
    .unwrap();
    assert_eq!(files.read("pipe").unwrap_err(), HttpError::new(415, "Only regular text files can be edited."));
    assert!(!files.list("").unwrap().entries.iter().any(|entry| entry.name == "pipe"));

    fs::write(f.workspace.join("latin1.txt"), [b'c', b'a', b'f', 0xe9]).unwrap();
    assert_eq!(files.read("latin1.txt").unwrap_err(), HttpError::new(415, "Only UTF-8 text files can be edited."));
    fs::write(f.workspace.join("escape.txt"), b"color \x1b[31m").unwrap();
    assert_eq!(files.read("escape.txt").unwrap_err(), HttpError::new(415, "Binary files cannot be edited."));
    fs::write(f.workspace.join("bom.txt"), b"\xef\xbb\xbf\xed\x95\x9c\xea\xb8\x80\n").unwrap();
    let bom = files.read("bom.txt").unwrap();
    assert_eq!(bom.content, "한글\n");
    assert_eq!(bom.hash, content_hash(b"\xef\xbb\xbf\xed\x95\x9c\xea\xb8\x80\n"));
    fs::write(f.workspace.join("exact.txt"), "y".repeat(MAX_FILE_BYTES)).unwrap();
    assert_eq!(files.read("exact.txt").unwrap().content.len(), MAX_FILE_BYTES);
    fs::write(f.workspace.join("nested/inner.txt"), "inner").unwrap();
    assert_eq!(files.read("nested/inner.txt").unwrap().path, "nested/inner.txt");
}

#[test]
fn saves_detect_conflicts_keep_modes_require_parents_and_leave_no_temporary_files() {
    let f = fixture();
    let files = WorkspaceFiles::new(&f.workspace);
    let original = files.read("notes.txt").unwrap();

    assert_eq!(files.save("notes.txt", "new", "").unwrap_err(), HttpError::conflict(CONFLICT));
    assert_eq!(files.save("fresh.txt", "new", &original.hash).unwrap_err(), HttpError::conflict(CONFLICT));
    assert_eq!(files.save("missing/child.txt", "new", "").unwrap_err(), HttpError::not_found("File or directory does not exist."));
    assert!(!f.workspace.join("missing").exists());
    assert_eq!(files.save("notes.txt", "a\0b", &original.hash).unwrap_err(), HttpError::new(415, "Binary files cannot be edited."));
    assert_eq!(
        files.save("notes.txt", &"z".repeat(MAX_FILE_BYTES + 1), &original.hash).unwrap_err(),
        HttpError::too_large("Text files are limited to 1 MiB.")
    );
    // Content validation runs before path validation, as in files.ts.
    assert_eq!(files.save("../x", "\u{1}", "").unwrap_err().status, 415);

    fs::write(f.workspace.join("binary.bin"), [0u8, 1]).unwrap();
    assert_eq!(files.save("binary.bin", "text", "").unwrap_err(), HttpError::new(415, "Binary files cannot be edited."));

    fs::set_permissions(f.workspace.join("notes.txt"), fs::Permissions::from_mode(0o640)).unwrap();
    let saved = files.save("notes.txt", "한국어 저장\n", &original.hash).unwrap();
    assert_eq!(fs::read_to_string(f.workspace.join("notes.txt")).unwrap(), "한국어 저장\n");
    assert_eq!(mode(&f.workspace.join("notes.txt")), 0o640);
    assert_eq!(files.expected("notes.txt").unwrap(), saved.hash);

    fs::create_dir(f.workspace.join("nested")).unwrap();
    files.save("nested/created.txt", "created\n", "").unwrap();
    assert_eq!(mode(&f.workspace.join("nested/created.txt")) & !0o600, 0);
    assert_eq!(fs::read_to_string(f.workspace.join("nested/created.txt")).unwrap(), "created\n");

    assert_eq!(names(&f.workspace), ["binary.bin", "nested", "notes.txt"]);
    assert_eq!(names(&f.workspace.join("nested")), ["created.txt"]);
}

#[test]
fn workspace_root_with_symlinked_ancestor_is_rejected() {
    let f = fixture();
    let alias = f.root.join("alias");
    symlink(&f.workspace, &alias).unwrap();
    let files = WorkspaceFiles::new(&alias);
    assert_eq!(files.list("").unwrap_err(), HttpError::forbidden(LINK_ERROR));
    assert_eq!(files.read("notes.txt").unwrap_err(), HttpError::forbidden(LINK_ERROR));
    assert_eq!(files.save("notes.txt", "x", "").unwrap_err(), HttpError::forbidden(LINK_ERROR));
    assert_eq!(fs::read_to_string(f.workspace.join("notes.txt")).unwrap(), "original\n");
}

#[test]
fn canonical_directory_validates_absolute_real_directories() {
    let f = fixture();
    let workspace = f.workspace.to_str().unwrap();
    assert_eq!(canonical_directory(workspace).unwrap(), f.workspace);
    assert_eq!(canonical_directory(&format!("{workspace}/")).unwrap(), f.workspace);
    assert_eq!(canonical_directory(&format!("{workspace}/./")).unwrap(), f.workspace);
    let invalid = HttpError::bad_request("Workspace must be an absolute directory path.");
    for input in ["relative/path", "", &format!("{workspace}/../workspace"), &format!("{workspace}\0")] {
        assert_eq!(canonical_directory(input).unwrap_err(), invalid, "{input:?}");
    }
    assert_eq!(canonical_directory(&format!("{workspace}/missing")).unwrap_err().status, 404);
    assert_eq!(canonical_directory(&format!("{workspace}/notes.txt")).unwrap_err(), HttpError::forbidden(LINK_ERROR));
    symlink(&f.workspace, f.root.join("alias")).unwrap();
    assert_eq!(
        canonical_directory(&format!("{}/alias/", f.root.display())).unwrap_err(),
        HttpError::forbidden(LINK_ERROR)
    );
}

#[test]
fn private_directories_are_created_0700_and_reject_links_and_root() {
    let f = fixture();
    let data = f.root.join("data/grok/conversation");
    assert_eq!(private_directory(&data).unwrap(), data);
    assert_eq!(mode(&data), 0o700);
    assert_eq!(mode(&f.root.join("data")), 0o700);
    assert_eq!(mode(&f.root.join("data/grok")), 0o700);

    let existing = f.root.join("existing");
    fs::create_dir(&existing).unwrap();
    fs::set_permissions(&existing, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(private_directory(&f.root.join("existing/./")).unwrap(), existing);
    assert_eq!(mode(&existing), 0o700);
    assert_eq!(private_directory(&f.root.join("existing/sub/../again")).unwrap(), existing.join("again"));

    assert_eq!(
        private_directory(Path::new("/")).unwrap_err(),
        HttpError::bad_request("Application data must use its own private directory.")
    );
    symlink(&existing, f.root.join("linked")).unwrap();
    assert_eq!(private_directory(&f.root.join("linked/child")).unwrap_err(), HttpError::forbidden(LINK_ERROR));
    assert!(!existing.join("child").exists());
    assert_eq!(private_directory(&f.root.join("linked")).unwrap_err(), HttpError::forbidden(LINK_ERROR));
    fs::write(f.root.join("file"), "").unwrap();
    assert_eq!(private_directory(&f.root.join("file/child")).unwrap_err(), HttpError::forbidden(LINK_ERROR));
}

#[test]
fn atomic_private_files_replace_contents_privately_without_following_links() {
    let f = fixture();
    let data = private_directory(&f.root.join("data")).unwrap();
    let state = data.join("state.json");
    atomic_private_file(&state, b"{\"revision\":1}").unwrap();
    assert_eq!(fs::read(&state).unwrap(), b"{\"revision\":1}");
    assert_eq!(mode(&state), 0o600);
    fs::set_permissions(&state, fs::Permissions::from_mode(0o644)).unwrap();
    atomic_private_file(&state, b"{\"revision\":2}").unwrap();
    assert_eq!(fs::read(&state).unwrap(), b"{\"revision\":2}");
    assert_eq!(mode(&state), 0o600);
    assert_eq!(names(&data), ["state.json"]);

    // A symbolic link at the target is replaced, never written through.
    let outside = f.root.join("outside.json");
    fs::write(&outside, "outside").unwrap();
    let link = data.join("link.json");
    symlink(&outside, &link).unwrap();
    atomic_private_file(&link, b"inside").unwrap();
    assert_eq!(fs::read_to_string(&outside).unwrap(), "outside");
    assert!(!fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    assert_eq!(fs::read_to_string(&link).unwrap(), "inside");

    symlink(&data, f.root.join("data-link")).unwrap();
    let error = atomic_private_file(&f.root.join("data-link/state.json"), b"blocked").unwrap_err();
    assert_eq!(io_error(error), HttpError::forbidden(LINK_ERROR));
    assert_eq!(fs::read(&state).unwrap(), b"{\"revision\":2}");

    let missing = atomic_private_file(&f.root.join("missing/state.json"), b"x").unwrap_err();
    assert!(grok_bot_files::is_missing(&missing));
    assert!(atomic_private_file(Path::new("relative.json"), b"x").is_err());

    // A directory at the target makes the rename fail and the temporary file is still removed.
    fs::create_dir(data.join("occupied")).unwrap();
    fs::write(data.join("occupied/child"), "").unwrap();
    assert!(atomic_private_file(&data.join("occupied"), b"x").is_err());
    assert_eq!(names(&data), ["link.json", "occupied", "state.json"]);
}
