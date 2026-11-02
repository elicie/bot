//! OAuth-only authenticated-encryption sidecar. This does not read or migrate legacy secrets.
//!
//! The random local master key is automatically kept beside the ciphertext in a separate 0600
//! file, beneath an owned 0700 directory. This protects against accidental plaintext disclosure,
//! not another process running as the same user, or theft of both the data and the key. There is
//! deliberately no configurable external key path or insecure/unavailable-vault fallback.

use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};
use std::sync::atomic::{AtomicBool, Ordering};

use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use serde::{Serialize, de::DeserializeOwned};
use zeroize::Zeroizing;

pub(super) const MAX_VAULT_BYTES: usize = 8 * 1024 * 1024;
const MAGIC: &[u8] = b"MCP-OAUTH-AES256GCM-V1\0";
const KEY: &str = "master.key";
const DATA: &str = "credentials.enc";
const DIRECTORY: &str = "mcp-oauth";
const SAFE_ERROR: &str = "The encrypted OAuth vault is unavailable. Credentials were not discarded.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct VaultError;

impl std::fmt::Display for VaultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(SAFE_ERROR)
    }
}
impl std::error::Error for VaultError {}

type Result<T> = std::result::Result<T, VaultError>;

/// All I/O is relative to a pinned directory descriptor; parents and targets never follow links.
/// The held nonblocking advisory lock prevents two application instances losing each other's data.
pub(super) struct Vault {
    directory: File,
    _lock: File,
    key: LessSafeKey,
    key_hash: [u8; 32],
    initialized: AtomicBool,
}

impl Vault {
    pub(super) fn open(root: &Path) -> Result<Self> {
        let root = walk(root)?;
        private_directory(&root)?;
        let name = name(DIRECTORY)?;
        // SAFETY: both the pinned descriptor and NUL-terminated constant name remain valid.
        let created = unsafe { libc::mkdirat(root.as_raw_fd(), name.as_ptr(), 0o700) };
        if created != 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists {
            return Err(VaultError);
        }
        let directory = open_at(&root, DIRECTORY, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        private_directory(&directory)?;
        if created == 0 {
            root.sync_all().map_err(|_| VaultError)?;
        }
        let lock = open_at(&directory, "lock", libc::O_RDWR | libc::O_CREAT, 0o600)?;
        private_file(&lock, 0)?;
        // SAFETY: flock only borrows the live file descriptor.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(VaultError);
        }
        let existing_data = read_at(&directory, DATA, MAX_VAULT_BYTES)?;
        let key_bytes = read_at(&directory, KEY, 32)?;
        let fresh = key_bytes.is_none() && existing_data.is_none();
        if key_bytes.is_some() && existing_data.is_none() {
            return Err(VaultError);
        }
        let bytes = match key_bytes {
            Some(bytes) if bytes.len() == 32 => Zeroizing::new(bytes),
            Some(_) => return Err(VaultError),
            None if existing_data.is_some() => return Err(VaultError),
            None => {
                let mut key = Zeroizing::new(vec![0u8; 32]);
                getrandom::fill(&mut key).map_err(|_| VaultError)?;
                atomic_write(&directory, KEY, &key)?;
                key
            }
        };
        let key_hash = ring::digest::digest(&ring::digest::SHA256, &bytes).as_ref().try_into().map_err(|_| VaultError)?;
        let key = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &bytes).map_err(|_| VaultError)?);
        Ok(Self { directory, _lock: lock, key, key_hash, initialized: AtomicBool::new(!fresh) })
    }

    pub(super) fn load<T: DeserializeOwned>(&self) -> Result<Option<T>> {
        private_directory(&self.directory)?;
        self.verify_key()?;
        let Some(bytes) = read_at(&self.directory, DATA, MAX_VAULT_BYTES)? else {
            return if self.initialized.load(Ordering::Acquire) { Err(VaultError) } else { Ok(None) };
        };
        if !bytes.starts_with(MAGIC) || bytes.len() < MAGIC.len() + 12 + AES_256_GCM.tag_len() {
            return Err(VaultError);
        }
        let nonce: [u8; 12] = bytes[MAGIC.len()..MAGIC.len() + 12].try_into().map_err(|_| VaultError)?;
        let mut ciphertext = Zeroizing::new(bytes[MAGIC.len() + 12..].to_vec());
        let plaintext = self.key.open_in_place(Nonce::assume_unique_for_key(nonce), Aad::from(MAGIC), &mut ciphertext).map_err(|_| VaultError)?;
        serde_json::from_slice(plaintext).map(Some).map_err(|_| VaultError)
    }

    pub(super) fn save<T: Serialize>(&self, value: &T) -> Result<()> {
        private_directory(&self.directory)?;
        self.verify_key()?;
        // Do not overwrite a deleted, tampered or unreadable existing encrypted store. A failure
        // requires operator recovery, never an implicit empty-vault reset.
        if self.initialized.load(Ordering::Acquire) {
            let previous: Option<serde::de::IgnoredAny> = self.load()?;
            if previous.is_none() {
                return Err(VaultError);
            }
        }
        let mut ciphertext = Zeroizing::new(serde_json::to_vec(value).map_err(|_| VaultError)?);
        if ciphertext.len() > MAX_VAULT_BYTES - MAGIC.len() - 12 - AES_256_GCM.tag_len() {
            return Err(VaultError);
        }
        let mut nonce = [0; 12];
        getrandom::fill(&mut nonce).map_err(|_| VaultError)?;
        self.key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(MAGIC), &mut *ciphertext).map_err(|_| VaultError)?;
        let mut bytes = Vec::with_capacity(MAGIC.len() + 12 + ciphertext.len());
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&nonce);
        bytes.extend_from_slice(&ciphertext);
        atomic_write(&self.directory, DATA, &bytes)?;
        self.initialized.store(true, Ordering::Release);
        Ok(())
    }

    fn verify_key(&self) -> Result<()> {
        let key = Zeroizing::new(read_at(&self.directory, KEY, 32)?.ok_or(VaultError)?);
        let hash = ring::digest::digest(&ring::digest::SHA256, &key);
        if key.len() != 32 || hash.as_ref() != self.key_hash {
            return Err(VaultError);
        }
        Ok(())
    }
}

fn name(value: &str) -> Result<CString> {
    CString::new(value).map_err(|_| VaultError)
}

fn walk(path: &Path) -> Result<File> {
    if !path.is_absolute() {
        return Err(VaultError);
    }
    let root = name("/")?;
    // SAFETY: open returns a new owned descriptor, checked before constructing File.
    let fd = unsafe { libc::open(root.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(VaultError);
    }
    let mut current = unsafe { File::from_raw_fd(fd) };
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(segment) => {
                let segment = segment.to_str().ok_or(VaultError)?;
                current = open_at(&current, segment, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
            }
            _ => return Err(VaultError),
        }
    }
    Ok(current)
}

fn open_at(directory: &File, path: &str, flags: i32, mode: libc::mode_t) -> Result<File> {
    let path = name(path)?;
    // SAFETY: the directory descriptor and path are valid; successful fd ownership is transferred.
    let fd = unsafe { libc::openat(directory.as_raw_fd(), path.as_ptr(), flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK, mode) };
    if fd < 0 { Err(VaultError) } else { Ok(unsafe { File::from_raw_fd(fd) }) }
}

fn private_directory(directory: &File) -> Result<()> {
    let metadata = directory.metadata().map_err(|_| VaultError)?;
    // SAFETY: geteuid has no pointer arguments or preconditions.
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o7777 != 0o700 {
        return Err(VaultError);
    }
    Ok(())
}

fn private_file(file: &File, limit: usize) -> Result<()> {
    let metadata = file.metadata().map_err(|_| VaultError)?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o600
        || metadata.len() > limit as u64
    {
        return Err(VaultError);
    }
    Ok(())
}

fn read_at(directory: &File, path: &str, limit: usize) -> Result<Option<Vec<u8>>> {
    let path_c = name(path)?;
    let fd = unsafe { libc::openat(directory.as_raw_fd(), path_c.as_ptr(), libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if fd < 0 {
        return if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound { Ok(None) } else { Err(VaultError) };
    }
    let file = unsafe { File::from_raw_fd(fd) };
    private_file(&file, limit)?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes).map_err(|_| VaultError)?;
    if bytes.len() > limit { Err(VaultError) } else { Ok(Some(bytes)) }
}

fn atomic_write(directory: &File, path: &str, bytes: &[u8]) -> Result<()> {
    // Validate any previous target rather than silently replacing a link or insecure file.
    let _ = read_at(directory, path, MAX_VAULT_BYTES)?;
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|_| VaultError)?;
    let temporary = format!(".{}.tmp", hex::encode(random));
    let temporary_c = name(&temporary)?;
    let destination = name(path)?;
    let result = (|| {
        let mut file = open_at(directory, &temporary, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL, 0o600)?;
        private_file(&file, 0)?;
        file.write_all(bytes).map_err(|_| VaultError)?;
        file.sync_all().map_err(|_| VaultError)?;
        // Both paths are constant/locally-generated names beneath the same pinned private dir.
        if path == KEY {
            // A master key is never replaced: linkat publishes the fully-synced temporary inode
            // atomically and fails if any destination already exists, including a raced link.
            if unsafe { libc::linkat(directory.as_raw_fd(), temporary_c.as_ptr(), directory.as_raw_fd(), destination.as_ptr(), 0) } != 0 {
                return Err(VaultError);
            }
            if unsafe { libc::unlinkat(directory.as_raw_fd(), temporary_c.as_ptr(), 0) } != 0 {
                return Err(VaultError);
            }
        } else if unsafe { libc::renameat(directory.as_raw_fd(), temporary_c.as_ptr(), directory.as_raw_fd(), destination.as_ptr()) } != 0 {
            return Err(VaultError);
        }
        directory.sync_all().map_err(|_| VaultError)
    })();
    if result.is_err() {
        // The exact randomly-generated temporary name above is the intended cleanup target.
        let _ = unsafe { libc::unlinkat(directory.as_raw_fd(), temporary_c.as_ptr(), 0) };
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn private_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[test]
    fn encrypted_roundtrip_reopen_and_no_plaintext() {
        let root = private_root();
        let vault = Vault::open(root.path()).unwrap();
        let tokens = vec!["synthetic-access-canary", "synthetic-refresh-canary"];
        vault.save(&tokens).unwrap();
        let bytes = std::fs::read(root.path().join(DIRECTORY).join(DATA)).unwrap();
        for value in &tokens {
            assert!(!bytes.windows(value.len()).any(|window| window == value.as_bytes()));
        }
        let first = bytes;
        vault.save(&tokens).unwrap();
        assert_ne!(first, std::fs::read(root.path().join(DIRECTORY).join(DATA)).unwrap());
        assert_eq!(std::fs::metadata(root.path().join(DIRECTORY).join(KEY)).unwrap().mode() & 0o777, 0o600);
        assert!(Vault::open(root.path()).is_err(), "concurrent writer must fail closed");
        drop(vault);
        let reopened = Vault::open(root.path()).unwrap();
        assert_eq!(reopened.load::<Vec<String>>().unwrap().unwrap(), tokens);
    }

    #[test]
    fn wrong_key_tamper_missing_key_and_malformed_plaintext_fail_closed() {
        for mode in 0..5 {
            let root = private_root();
            let vault = Vault::open(root.path()).unwrap();
            vault.save(&vec!["synthetic-canary"]).unwrap();
            drop(vault);
            let key = root.path().join(DIRECTORY).join(KEY);
            let data = root.path().join(DIRECTORY).join(DATA);
            match mode {
                0 => std::fs::write(&key, [7u8; 32]).unwrap(),
                1 => {
                    let mut bytes = std::fs::read(&data).unwrap();
                    *bytes.last_mut().unwrap() ^= 1;
                    std::fs::write(&data, bytes).unwrap();
                }
                2 => std::fs::remove_file(&key).unwrap(),
                3 => std::fs::remove_file(&data).unwrap(),
                _ => std::fs::write(&data, b"{\"token\":\"synthetic-canary\"}").unwrap(),
            }
            let result = Vault::open(root.path()).and_then(|vault| vault.load::<Vec<String>>());
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().to_string(), SAFE_ERROR);
            assert_eq!(data.exists(), mode != 3, "never delete failed encrypted input");
        }
    }

    #[test]
    fn live_tamper_key_replacement_and_missing_ciphertext_are_never_overwritten() {
        for mode in 0..3 {
            let root = private_root();
            let vault = Vault::open(root.path()).unwrap();
            vault.save(&vec!["synthetic-preserved-token"]).unwrap();
            let data = root.path().join(DIRECTORY).join(DATA);
            let key = root.path().join(DIRECTORY).join(KEY);
            match mode {
                0 => {
                    let mut bytes = std::fs::read(&data).unwrap();
                    *bytes.last_mut().unwrap() ^= 1;
                    std::fs::write(&data, bytes).unwrap();
                }
                1 => std::fs::write(&key, [9u8; 32]).unwrap(),
                _ => std::fs::remove_file(&data).unwrap(),
            }
            let before = std::fs::read(&data).ok();
            assert_eq!(vault.save(&vec!["must-not-replace"]).err(), Some(VaultError));
            assert_eq!(std::fs::read(&data).ok(), before);
        }
    }

    #[test]
    fn links_permissions_and_insecure_parents_are_rejected() {
        let root = private_root();
        let elsewhere = root.path().join("elsewhere");
        std::fs::write(&elsewhere, [1u8; 32]).unwrap();
        std::fs::create_dir(root.path().join(DIRECTORY)).unwrap();
        std::fs::set_permissions(root.path().join(DIRECTORY), std::fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&elsewhere, root.path().join(DIRECTORY).join(KEY)).unwrap();
        assert!(Vault::open(root.path()).is_err());
        let other = private_root();
        let vault = Vault::open(other.path()).unwrap();
        vault.save(&vec!["canary"]).unwrap();
        drop(vault);
        let data = other.path().join(DIRECTORY).join(DATA);
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Vault::open(other.path()).is_err());
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::hard_link(&data, other.path().join("hardlink")).unwrap();
        assert!(Vault::open(other.path()).is_err());
        let linked = root.path().join("linked-parent");
        symlink(other.path(), &linked).unwrap();
        assert!(Vault::open(&linked).is_err());
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Vault::open(root.path()).is_err());
    }
}
