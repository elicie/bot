//! Images a provider model generated in a reply (OpenAI-compatible `images` on the assistant
//! message or stream delta). The provider relay stores each one here under a random UUID and puts a
//! Markdown reference to `GET /api/generated-images/:id` into the reply text, so the stored message
//! carries the link and the engine sees visible content.
//!
//! Files live in `<data>/generated-images/<uuid>` (0600, no extension; the type is sniffed from the
//! signature when served). They are not tied to a conversation when written, so cleanup is a sweep:
//! files that no stored message references and that are older than a grace period (a reply still
//! streaming has not been committed yet) are removed.

use std::collections::HashSet;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use grok_bot_model::HttpError;

use crate::attachments::image_type;
use crate::validate::is_uuid;

/// Largest generated image kept (decoded bytes).
pub const MAX_GENERATED_IMAGE_BYTES: usize = 20 * 1024 * 1024;
/// URL path prefix of the serving route; also what message text is scanned for.
pub const ROUTE_PREFIX: &str = "/api/generated-images/";

fn directory(data_dir: &Path) -> PathBuf {
    data_dir.join("generated-images")
}

/// Store decoded image bytes; returns the new ID. Only PNG, JPEG, GIF, and WebP are kept.
pub fn save_generated_image(data_dir: &Path, bytes: &[u8]) -> Result<String, String> {
    if bytes.len() > MAX_GENERATED_IMAGE_BYTES {
        return Err("generated image is too large".into());
    }
    if image_type(bytes).is_none() {
        return Err("generated image is not PNG, JPEG, GIF, or WebP".into());
    }
    let directory = grok_bot_files::private_directory(&directory(data_dir)).map_err(|error| error.message)?;
    let id = uuid::Uuid::new_v4().to_string();
    grok_bot_files::atomic_private_file(&directory.join(&id), bytes).map_err(|error| error.to_string())?;
    Ok(id)
}

/// Read one stored image and its MIME type (404 when missing or not a regular single-link file).
pub fn read_generated_image(data_dir: &Path, id: &str) -> Result<(Vec<u8>, &'static str), HttpError> {
    let missing = || HttpError::not_found("Image does not exist.");
    if !is_uuid(id) {
        return Err(missing());
    }
    let mut file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(directory(data_dir).join(id)).map_err(|_| missing())?;
    let metadata = file.metadata().map_err(|_| missing())?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.len() > MAX_GENERATED_IMAGE_BYTES as u64 {
        return Err(missing());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes).map_err(|_| missing())?;
    let mime = image_type(&bytes).ok_or_else(missing)?;
    Ok((bytes, mime))
}

/// Generated-image IDs referenced by `text`.
pub fn referenced_ids(text: &str) -> impl Iterator<Item = &str> {
    text.match_indices(ROUTE_PREFIX).filter_map(move |(at, _)| {
        let id = text.get(at + ROUTE_PREFIX.len()..at + ROUTE_PREFIX.len() + 36)?;
        is_uuid(id).then_some(id)
    })
}

/// Remove stored images that `referenced` does not contain and that are older than `grace`.
/// Best effort; returns how many files were removed.
pub fn prune_generated_images(data_dir: &Path, referenced: &HashSet<String>, grace: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(directory(data_dir)) else { return 0 };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_uuid(name) || referenced.contains(name) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else { continue };
        let old = metadata.modified().ok().and_then(|modified| now.duration_since(modified).ok()).is_some_and(|age| age >= grace);
        if metadata.is_file() && old && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n0000";

    #[test]
    fn saves_reads_finds_and_prunes() {
        let root = tempfile::tempdir().unwrap();
        assert!(save_generated_image(root.path(), b"<svg/>").is_err());
        let kept = save_generated_image(root.path(), PNG).unwrap();
        let dropped = save_generated_image(root.path(), PNG).unwrap();
        assert_eq!(read_generated_image(root.path(), &kept).unwrap(), (PNG.to_vec(), "image/png"));
        assert!(read_generated_image(root.path(), "../x").is_err());
        let text = format!("a ![img]({ROUTE_PREFIX}{kept}) b {ROUTE_PREFIX}not-a-uuid");
        let ids: HashSet<String> = referenced_ids(&text).map(str::to_owned).collect();
        assert_eq!(ids, HashSet::from([kept.clone()]));
        // Inside the grace period nothing is removed; afterwards only the unreferenced file is.
        assert_eq!(prune_generated_images(root.path(), &ids, Duration::from_secs(3600)), 0);
        assert_eq!(prune_generated_images(root.path(), &ids, Duration::ZERO), 1);
        assert!(read_generated_image(root.path(), &kept).is_ok());
        assert!(read_generated_image(root.path(), &dropped).is_err());
    }
}
