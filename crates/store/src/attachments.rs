//! User message attachments: upload validation and limits, private blob storage under the data
//! directory, and the persisted-metadata checks `state.json` loading applies.
//!
//! `state.json` keeps only metadata ([`Attachment`]) on the user message. The bytes live in
//! `<data dir>/attachments/<conversation id>/<attachment id>` (owner-only directory and files), so
//! images never inflate the 64 MiB state file. Removing a conversation removes its directory.

use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use grok_bot_model::{Attachment, AttachmentKind, AttachmentUpload, HttpError, HttpResult, new_id};

use crate::validate::{is_uuid, js_len, js_trim};

/// Files per message.
pub const MAX_ATTACHMENTS: usize = 8;
/// One text file (UTF-8 bytes). Text is delivered inline with the prompt.
pub const MAX_TEXT_ATTACHMENT_BYTES: usize = 256 * 1024;
/// All text files of one message together.
pub const MAX_TEXT_ATTACHMENTS_TOTAL_BYTES: usize = 512 * 1024;
/// One image file.
pub const MAX_IMAGE_ATTACHMENT_BYTES: usize = 5 * 1024 * 1024;
/// All files of one message together.
pub const MAX_ATTACHMENTS_TOTAL_BYTES: usize = 10 * 1024 * 1024;
/// Request body limit of the attachment message route: base64 of the total plus JSON overhead.
pub const MAX_ATTACHMENT_REQUEST_BYTES: usize = MAX_ATTACHMENTS_TOTAL_BYTES / 3 * 4 + 2 * 1024 * 1024;
pub const IMAGE_MIME_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];
const MAX_NAME_UNITS: usize = 255;

/// A validated upload: metadata for the message plus the decoded bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedAttachment {
    pub meta: Attachment,
    pub bytes: Vec<u8>,
}

impl PreparedAttachment {
    /// UTF-8 contents of a text attachment.
    pub fn text(&self) -> Option<&str> {
        (self.meta.kind == AttachmentKind::Text).then(|| std::str::from_utf8(&self.bytes).ok()).flatten()
    }
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8) | u32::from(*chunk.get(2).unwrap_or(&0));
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * index) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Standard base64 with optional padding; whitespace is not accepted.
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let trimmed = match bytes {
        [rest @ .., b'=', b'='] => rest,
        [rest @ .., b'='] => rest,
        _ => bytes,
    };
    if trimmed.len() % 4 == 1 || (trimmed.len() != bytes.len() && !bytes.len().is_multiple_of(4)) {
        return None;
    }
    let mut out = Vec::with_capacity(trimmed.len() / 4 * 3 + 2);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for &byte in trimmed {
        let value = ALPHABET.iter().position(|&c| c == byte)? as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    // Leftover bits must be zero padding (canonical encoding).
    (buffer == 0).then_some(out)
}

/// The image type the bytes actually are, from their signature.
pub fn image_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// `1 MB`-style label used in limit messages.
pub fn size_label(bytes: usize) -> String {
    if bytes >= 1024 * 1024 && bytes.is_multiple_of(1024 * 1024) {
        format!("{} MB", bytes / (1024 * 1024))
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}

/// The file name without any directory part, trimmed; control characters are refused.
fn clean_name(name: &str) -> HttpResult<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let base = js_trim(base);
    if base.is_empty() || js_len(base) > MAX_NAME_UNITS || base.chars().any(char::is_control) {
        return Err(HttpError::bad_request("attachments: Each file needs a name of at most 255 characters without control characters."));
    }
    Ok(base.to_owned())
}

/// MIME type recorded for a text attachment: the declared `text/*` or common structured-text
/// type, otherwise `text/plain`.
fn text_mime(declared: &str) -> String {
    let declared = declared.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    let structured = ["application/json", "application/xml", "application/yaml", "application/x-yaml", "application/toml", "application/javascript", "application/x-sh", "application/sql"];
    let valid_token = !declared.is_empty() && declared.len() <= 100 && declared.bytes().all(|b| b.is_ascii_alphanumeric() || b"/.+-_".contains(&b));
    if valid_token && (declared.starts_with("text/") || structured.contains(&declared.as_str())) { declared } else { "text/plain".into() }
}

/// Validate one message's uploads: count, per-file and total size limits, UTF-8 text or a PNG,
/// JPEG, GIF, or WebP image whose bytes match its type. Limits answer 413; malformed input 400.
pub fn prepare_attachments(uploads: Vec<AttachmentUpload>) -> HttpResult<Vec<PreparedAttachment>> {
    if uploads.len() > MAX_ATTACHMENTS {
        return Err(HttpError::bad_request(format!("Attach at most {MAX_ATTACHMENTS} files to one message.")));
    }
    let mut prepared = Vec::with_capacity(uploads.len());
    let (mut total, mut text_total) = (0usize, 0usize);
    for upload in uploads {
        let name = clean_name(&upload.name)?;
        if upload.data.len() > MAX_IMAGE_ATTACHMENT_BYTES.div_ceil(3) * 4 + 4 {
            return Err(HttpError::too_large(format!("\"{name}\" is too large to attach (maximum {}).", size_label(MAX_IMAGE_ATTACHMENT_BYTES))));
        }
        let bytes = base64_decode(&upload.data).ok_or_else(|| HttpError::bad_request(format!("\"{name}\" was not sent as valid base64 data.")))?;
        if bytes.is_empty() {
            return Err(HttpError::bad_request(format!("\"{name}\" is empty and was not attached.")));
        }
        let declared = upload.mime_type.trim().to_ascii_lowercase();
        let (kind, mime_type) = if declared.starts_with("image/") || image_type(&bytes).is_some() {
            let Some(actual) = image_type(&bytes) else {
                return Err(HttpError::bad_request(format!("\"{name}\" is not a PNG, JPEG, GIF, or WebP image.")));
            };
            if bytes.len() > MAX_IMAGE_ATTACHMENT_BYTES {
                return Err(HttpError::too_large(format!("\"{name}\" is too large to attach (maximum {}).", size_label(MAX_IMAGE_ATTACHMENT_BYTES))));
            }
            (AttachmentKind::Image, actual.to_owned())
        } else {
            if std::str::from_utf8(&bytes).is_err() || bytes.contains(&0) {
                return Err(HttpError::bad_request(format!("\"{name}\" is not a UTF-8 text file or a PNG, JPEG, GIF, or WebP image.")));
            }
            if bytes.len() > MAX_TEXT_ATTACHMENT_BYTES {
                return Err(HttpError::too_large(format!("\"{name}\" is too large to attach (maximum {} for text files).", size_label(MAX_TEXT_ATTACHMENT_BYTES))));
            }
            text_total += bytes.len();
            (AttachmentKind::Text, text_mime(&declared))
        };
        total += bytes.len();
        prepared.push(PreparedAttachment {
            meta: Attachment { id: new_id(), name, mime_type, kind, size: bytes.len() as u64 },
            bytes,
        });
    }
    if text_total > MAX_TEXT_ATTACHMENTS_TOTAL_BYTES {
        return Err(HttpError::too_large(format!("Text attachments of one message may total at most {}.", size_label(MAX_TEXT_ATTACHMENTS_TOTAL_BYTES))));
    }
    if total > MAX_ATTACHMENTS_TOTAL_BYTES {
        return Err(HttpError::too_large(format!("Attachments of one message may total at most {}.", size_label(MAX_ATTACHMENTS_TOTAL_BYTES))));
    }
    Ok(prepared)
}

/// Persisted metadata check used when loading `state.json`.
pub(crate) fn attachments_valid(attachments: &[Attachment]) -> bool {
    let mut total = 0u64;
    attachments.len() <= MAX_ATTACHMENTS
        && attachments.iter().all(|attachment| {
            total += attachment.size;
            let limit = match attachment.kind {
                AttachmentKind::Text => MAX_TEXT_ATTACHMENT_BYTES,
                AttachmentKind::Image => MAX_IMAGE_ATTACHMENT_BYTES,
            };
            is_uuid(&attachment.id)
                && (1..=MAX_NAME_UNITS).contains(&js_len(&attachment.name))
                && !attachment.name.contains(['/', '\\'])
                && !attachment.name.chars().any(char::is_control)
                && (1..=limit as u64).contains(&attachment.size)
                && match attachment.kind {
                    AttachmentKind::Image => IMAGE_MIME_TYPES.contains(&attachment.mime_type.as_str()),
                    AttachmentKind::Text => text_mime(&attachment.mime_type) == attachment.mime_type,
                }
        })
        && total <= MAX_ATTACHMENTS_TOTAL_BYTES as u64
}

fn conversation_directory(data_dir: &Path, conversation_id: &str) -> HttpResult<PathBuf> {
    if !is_uuid(conversation_id) {
        return Err(HttpError::not_found("Attachment does not exist."));
    }
    Ok(data_dir.join("attachments").join(conversation_id))
}

/// Write the blobs of one message before the message is committed.
pub fn write_attachments(data_dir: &Path, conversation_id: &str, attachments: &[PreparedAttachment]) -> HttpResult<()> {
    if attachments.is_empty() {
        return Ok(());
    }
    grok_bot_files::private_directory(&data_dir.join("attachments"))?;
    let directory = grok_bot_files::private_directory(&conversation_directory(data_dir, conversation_id)?)?;
    for attachment in attachments {
        grok_bot_files::atomic_private_file(&directory.join(&attachment.meta.id), &attachment.bytes).map_err(grok_bot_files::io_error)?;
    }
    Ok(())
}

/// Read one stored blob (404 when missing; links and non-regular files are refused).
pub fn read_attachment(data_dir: &Path, conversation_id: &str, attachment_id: &str) -> HttpResult<Vec<u8>> {
    let missing = || HttpError::not_found("Attachment does not exist.");
    if !is_uuid(attachment_id) {
        return Err(missing());
    }
    let path = conversation_directory(data_dir, conversation_id)?.join(attachment_id);
    let mut file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path).map_err(|_| missing())?;
    let metadata = file.metadata().map_err(|_| missing())?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.len() > MAX_IMAGE_ATTACHMENT_BYTES as u64 {
        return Err(missing());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes).map_err(|_| missing())?;
    Ok(bytes)
}

/// Best-effort removal of blobs whose message was never committed.
pub fn discard_attachments(data_dir: &Path, conversation_id: &str, attachments: &[Attachment]) {
    let Ok(directory) = conversation_directory(data_dir, conversation_id) else { return };
    for attachment in attachments {
        if is_uuid(&attachment.id) {
            let _ = std::fs::remove_file(directory.join(&attachment.id));
        }
    }
}

/// Best-effort removal of everything kept outside `state.json` for a deleted conversation: its
/// attachment blobs and its private engine session directories.
pub fn remove_conversation_files(data_dir: &Path, conversation_id: &str) {
    if !is_uuid(conversation_id) {
        return;
    }
    let _ = std::fs::remove_dir_all(data_dir.join("attachments").join(conversation_id));
    let _ = std::fs::remove_dir_all(data_dir.join("grok").join(conversation_id));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_and_rejects_noncanonical_input() {
        for sample in [&b""[..], b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar", &[0, 255, 128, 7]] {
            let encoded = base64_encode(sample);
            assert_eq!(base64_decode(&encoded).as_deref(), Some(sample), "{encoded}");
        }
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_decode("Zm8").as_deref(), Some(&b"fo"[..]));
        assert!(base64_decode("Zm9=").is_none(), "nonzero padding bits");
        assert!(base64_decode("Z").is_none());
        assert!(base64_decode("Zm 8=").is_none());
        assert!(base64_decode("Zm8=Zm8=").is_none());
    }

    #[test]
    fn sizes_and_types() {
        assert_eq!(size_label(5 * 1024 * 1024), "5 MB");
        assert_eq!(size_label(256 * 1024), "256 KB");
        assert_eq!(text_mime("text/markdown; charset=utf-8"), "text/markdown");
        assert_eq!(text_mime("application/octet-stream"), "text/plain");
        assert_eq!(text_mime(""), "text/plain");
        assert_eq!(image_type(b"GIF89a...."), Some("image/gif"));
        assert_eq!(image_type(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(clean_name("../../etc/passwd").unwrap(), "passwd");
        assert_eq!(clean_name("C:\\Users\\me\\notes.txt").unwrap(), "notes.txt");
        assert!(clean_name("bad\nname").is_err());
        assert!(clean_name("dir/").is_err());
    }
}
