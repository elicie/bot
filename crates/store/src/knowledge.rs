//! Per-bot knowledge files ("지식"): UTF-8 text the bot can read as shared context. Metadata lives
//! in `state.json`; contents live in `<data>/knowledge/<botId>/<fileId>.txt` (directories 0700,
//! files 0600, written atomically). Both are bounded so the state stays far below its 64 MiB limit.

use std::collections::HashSet;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use grok_bot_model::{HttpError, HttpResult, KnowledgeFile, KnowledgeInput, iso, new_id};

use crate::validate::{Issues, is_utc_timestamp, is_uuid, js_trim};
use crate::{State, Store};

pub const MAX_KNOWLEDGE_FILES_PER_BOT: usize = 50;
pub const MAX_KNOWLEDGE_FILE_BYTES: usize = 512 * 1024;
pub const MAX_KNOWLEDGE_BYTES_PER_BOT: u64 = 8 * 1024 * 1024;
pub const MAX_KNOWLEDGE_NAME_CHARACTERS: usize = 200;

const MISSING: &str = "Knowledge file does not exist for this bot.";
const UNREADABLE: &str = "Knowledge file contents are missing or changed on disk. Delete the file and add it again.";

/// `<data>/knowledge`.
pub fn knowledge_root(data_dir: &Path) -> PathBuf {
    data_dir.join("knowledge")
}

fn content_path(data_dir: &Path, bot_id: &str, id: &str) -> PathBuf {
    knowledge_root(data_dir).join(bot_id).join(format!("{id}.txt"))
}

fn name_issues(issues: &mut Issues, name: &mut String) {
    *name = js_trim(name).to_owned();
    issues.length("name", name, 1, MAX_KNOWLEDGE_NAME_CHARACTERS);
    if name.chars().any(|c| c.is_control() || matches!(c, '/' | '\\')) {
        issues.push("name", "File names cannot contain control characters or path separators.");
    }
}

/// Validate a knowledge upload: a display name and nonempty UTF-8 text of at most 512 KiB.
pub fn validate_knowledge_input(mut input: KnowledgeInput) -> HttpResult<KnowledgeInput> {
    let mut issues = Issues::new();
    name_issues(&mut issues, &mut input.name);
    if js_trim(&input.content).is_empty() || input.content.contains('\0') {
        issues.push("content", "Provide nonempty UTF-8 text without NUL bytes.");
    }
    issues.finish()?;
    if input.content.len() > MAX_KNOWLEDGE_FILE_BYTES {
        return Err(HttpError::too_large("Knowledge files must be at most 512 KiB of UTF-8 text."));
    }
    Ok(input)
}

pub(crate) fn persisted_knowledge_valid(files: &[KnowledgeFile]) -> bool {
    files.len() <= MAX_KNOWLEDGE_FILES_PER_BOT
        && files.iter().map(|file| file.id.as_str()).collect::<HashSet<_>>().len() == files.len()
        && files.iter().map(|file| file.bytes).sum::<u64>() <= MAX_KNOWLEDGE_BYTES_PER_BOT
        && files.iter().all(|file| {
            let mut issues = Issues::new();
            let mut name = file.name.clone();
            name_issues(&mut issues, &mut name);
            issues.is_empty()
                && name == file.name
                && is_uuid(&file.id)
                && file.bytes <= MAX_KNOWLEDGE_FILE_BYTES as u64
                && file.hash.len() == 64
                && file.hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                && is_utc_timestamp(&file.created_at)
        })
}

fn io_failure() -> HttpError {
    HttpError::new(500, "Knowledge file could not be stored. Check that the data directory is accessible.")
}

impl State {
    pub fn knowledge(&self, bot_id: &str) -> HttpResult<&[KnowledgeFile]> {
        Ok(self.bot(bot_id)?.knowledge.as_deref().unwrap_or_default())
    }

    pub fn knowledge_file(&self, bot_id: &str, id: &str) -> HttpResult<&KnowledgeFile> {
        self.knowledge(bot_id)?.iter().find(|file| file.id == id).ok_or_else(|| HttpError::not_found(MISSING))
    }

    /// The checks `insert_knowledge` repeats, run before contents are written.
    fn knowledge_admissible(&self, bot_id: &str, name: &str, bytes: u64) -> HttpResult<usize> {
        self.assert_idle()?;
        let index = self.bot_index(bot_id)?;
        let files = self.knowledge(bot_id)?;
        if files.len() >= MAX_KNOWLEDGE_FILES_PER_BOT {
            return Err(HttpError::conflict("A bot can hold at most 50 knowledge files. Delete one before adding more."));
        }
        if files.iter().any(|file| file.name.to_lowercase() == name.to_lowercase()) {
            return Err(HttpError::conflict("A knowledge file with this name already exists for the bot."));
        }
        if files.iter().map(|file| file.bytes).sum::<u64>() + bytes > MAX_KNOWLEDGE_BYTES_PER_BOT {
            return Err(HttpError::too_large("Knowledge files are limited to 8 MiB per bot. Delete a file before adding more."));
        }
        Ok(index)
    }

    fn insert_knowledge(&mut self, bot_id: &str, file: KnowledgeFile) -> HttpResult<()> {
        let index = self.knowledge_admissible(bot_id, &file.name, file.bytes)?;
        let mut candidate = self.disk.bots[index].clone();
        candidate.knowledge.get_or_insert_with(Vec::new).push(file);
        {
            let mut prospect = self.prospect();
            prospect.bots[index] = &candidate;
            Self::check_capacity(&prospect)?;
        }
        self.disk_mut().bots[index] = candidate;
        Ok(())
    }

    fn remove_knowledge(&mut self, bot_id: &str, id: &str) -> HttpResult<KnowledgeFile> {
        self.assert_idle()?;
        let index = self.bot_index(bot_id)?;
        let file = self.knowledge_file(bot_id, id)?.clone();
        let bot = &mut self.disk_mut().bots[index];
        if let Some(files) = bot.knowledge.as_mut() {
            files.retain(|item| item.id != id);
            if files.is_empty() {
                bot.knowledge = None;
            }
        }
        Ok(file)
    }
}

fn write_contents(data_dir: &Path, bot_id: &str, id: &str, content: &[u8]) -> HttpResult<()> {
    let root = grok_bot_files::private_directory(&knowledge_root(data_dir))?;
    grok_bot_files::private_directory(&root.join(bot_id))?;
    grok_bot_files::atomic_private_file(&content_path(data_dir, bot_id, id), content).map_err(|_| io_failure())
}

fn read_contents(path: &Path, file: &KnowledgeFile) -> HttpResult<String> {
    let unreadable = || HttpError::conflict(UNREADABLE);
    let mut handle = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path).map_err(|_| unreadable())?;
    let metadata = handle.metadata().map_err(|_| unreadable())?;
    if !metadata.is_file() || metadata.len() != file.bytes {
        return Err(unreadable());
    }
    let mut bytes = Vec::with_capacity(file.bytes as usize);
    handle.by_ref().take(MAX_KNOWLEDGE_FILE_BYTES as u64 + 1).read_to_end(&mut bytes).map_err(|_| unreadable())?;
    if grok_bot_files::content_hash(&bytes) != file.hash {
        return Err(unreadable());
    }
    String::from_utf8(bytes).map_err(|_| unreadable())
}

/// Remove contents without metadata (an interrupted add) and directories of deleted bots.
fn prune(data_dir: &Path, keep: &[(String, Vec<String>)]) {
    let root = knowledge_root(data_dir);
    if !std::fs::symlink_metadata(&root).is_ok_and(|metadata| metadata.is_dir()) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(&root) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((_, files)) = keep.iter().find(|(bot, _)| *bot == name) else {
            let _ = if entry.file_type().is_ok_and(|kind| kind.is_dir()) { std::fs::remove_dir_all(entry.path()) } else { std::fs::remove_file(entry.path()) };
            continue;
        };
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Ok(children) = std::fs::read_dir(entry.path()) else { continue };
        for child in children.flatten() {
            let child_name = child.file_name().to_string_lossy().into_owned();
            if !files.iter().any(|id| child_name == format!("{id}.txt")) {
                let _ = std::fs::remove_file(child.path());
            }
        }
    }
}

impl Store {
    /// Store a knowledge file: contents first (private, atomic), then its metadata. A failed
    /// metadata commit removes the contents again.
    pub async fn add_knowledge(&self, bot_id: &str, input: KnowledgeInput) -> HttpResult<KnowledgeFile> {
        let input = validate_knowledge_input(input)?;
        if !is_uuid(bot_id) {
            return Err(HttpError::not_found("Bot does not exist."));
        }
        let bytes = input.content.len() as u64;
        self.read(|state| state.knowledge_admissible(bot_id, &input.name, bytes))?;
        let file =
            KnowledgeFile { id: new_id(), name: input.name, bytes, hash: grok_bot_files::content_hash(input.content.as_bytes()), created_at: iso(self.now()) };
        let data_dir = self.data_dir().to_path_buf();
        let (bot, id) = (bot_id.to_owned(), file.id.clone());
        let content = input.content;
        tokio::task::spawn_blocking(move || write_contents(&data_dir, &bot, &id, content.as_bytes())).await.map_err(|_| io_failure())??;
        let stored = file.clone();
        let bot = bot_id.to_owned();
        if let Err(error) = self.mutate(move |state| state.insert_knowledge(&bot, stored)).await {
            let _ = std::fs::remove_file(content_path(self.data_dir(), bot_id, &file.id));
            return Err(error);
        }
        Ok(file)
    }

    /// Delete a knowledge file's metadata, then its contents.
    pub async fn delete_knowledge(&self, bot_id: &str, id: &str) -> HttpResult<KnowledgeFile> {
        let (bot, file_id) = (bot_id.to_owned(), id.to_owned());
        let file = self.mutate(move |state| state.remove_knowledge(&bot, &file_id)).await?;
        let path = content_path(self.data_dir(), bot_id, &file.id);
        let _ = tokio::task::spawn_blocking(move || std::fs::remove_file(path)).await;
        Ok(file)
    }

    /// Metadata and complete contents of one knowledge file, verified against its stored hash.
    pub async fn read_knowledge(&self, bot_id: &str, id: &str) -> HttpResult<(KnowledgeFile, String)> {
        let file = self.read(|state| state.knowledge_file(bot_id, id).cloned())?;
        let path = content_path(self.data_dir(), bot_id, &file.id);
        let metadata = file.clone();
        let content = tokio::task::spawn_blocking(move || read_contents(&path, &metadata)).await.map_err(|_| io_failure())??;
        Ok((file, content))
    }

    /// Remove knowledge contents no bot references. Called once when the store opens.
    pub(crate) async fn prune_knowledge(&self) {
        let keep: Vec<(String, Vec<String>)> = self
            .read(|state| state.disk().bots.iter().map(|bot| (bot.id.clone(), bot.knowledge.iter().flatten().map(|file| file.id.clone()).collect())).collect());
        let data_dir = self.data_dir().to_path_buf();
        let _ = tokio::task::spawn_blocking(move || prune(&data_dir, &keep)).await;
    }
}
