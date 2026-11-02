//! Chrome process and a minimal Chrome DevTools Protocol client over `--remote-debugging-pipe`.
//!
//! The pipe transport (file descriptors 3 and 4 of the Chrome process, NUL-terminated JSON) opens
//! no debugging port, so no other local process can attach to the managed browser. This is the
//! transport Playwright used for the Node implementation.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::pipe;
use tokio::sync::{mpsc, oneshot};

/// A CDP command failed or the browser connection closed.
#[derive(Debug)]
pub(crate) struct CdpError;

pub(crate) type CdpResult<T = Value> = Result<T, CdpError>;

/// A protocol event; `session` is the flattened target session it belongs to.
#[derive(Clone, Debug)]
pub(crate) struct Event {
    pub method: String,
    pub params: Value,
    pub session: Option<String>,
}

pub(crate) struct Connection {
    writer: tokio::sync::Mutex<pipe::Sender>,
    pending: Mutex<HashMap<u64, oneshot::Sender<CdpResult>>>,
    next: AtomicU64,
    closed: std::sync::atomic::AtomicBool,
}

impl Connection {
    fn pending(&self) -> MutexGuard<'_, HashMap<u64, oneshot::Sender<CdpResult>>> {
        self.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Send one command and wait for its result.
    pub async fn send(&self, session: Option<&str>, method: &str, params: Value) -> CdpResult {
        if self.closed.load(Ordering::SeqCst) {
            return Err(CdpError);
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        let (resolve, result) = oneshot::channel();
        self.pending().insert(id, resolve);
        let mut bytes = message.to_string().into_bytes();
        bytes.push(0);
        let written = {
            let mut writer = self.writer.lock().await;
            writer.write_all(&bytes).await
        };
        if written.is_err() {
            self.pending().remove(&id);
            return Err(CdpError);
        }
        result.await.unwrap_or(Err(CdpError))
    }

    fn dispatch(&self, message: Value, events: &mpsc::UnboundedSender<Event>) {
        if let Some(id) = message.get("id").and_then(Value::as_u64) {
            if let Some(resolve) = self.pending().remove(&id) {
                let result = match message.get("error") {
                    Some(_) => Err(CdpError),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = resolve.send(result);
            }
            return;
        }
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            let _ = events.send(Event {
                method: method.to_owned(),
                params: message.get("params").cloned().unwrap_or(Value::Null),
                session: message.get("sessionId").and_then(Value::as_str).map(str::to_owned),
            });
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        for (_, resolve) in self.pending().drain() {
            let _ = resolve.send(Err(CdpError));
        }
    }
}

/// The managed Chrome process. Dropping it kills the process.
pub(crate) struct Chrome {
    pub connection: Arc<Connection>,
    child: tokio::sync::Mutex<Option<tokio::process::Child>>,
    reader: tokio::task::AbortHandle,
}

impl Drop for Chrome {
    fn drop(&mut self) {
        self.reader.abort();
        if let Ok(mut child) = self.child.try_lock()
            && let Some(child) = child.as_mut()
        {
            let _ = child.start_kill();
        }
    }
}

/// Executable of the stable Google Chrome channel (Playwright `channel: 'chrome'`).
pub(crate) fn chrome_executable() -> PathBuf {
    if cfg!(target_os = "macos") {
        PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
    } else {
        PathBuf::from("/opt/google/chrome/chrome")
    }
}

fn pipe_pair() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: `pipe2`/`pipe` write two new descriptors into `fds` on success.
    #[cfg(target_os = "linux")]
    let created = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    #[cfg(not(target_os = "linux"))]
    let created = unsafe {
        let created = libc::pipe(fds.as_mut_ptr());
        if created == 0 {
            libc::fcntl(fds[0], libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(fds[1], libc::F_SETFD, libc::FD_CLOEXEC);
        }
        created
    };
    if created != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just created and are owned here.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Duplicate `fd` above the descriptors the child uses (3 and 4), close-on-exec.
fn high(fd: OwnedFd) -> std::io::Result<OwnedFd> {
    // SAFETY: F_DUPFD_CLOEXEC returns a new descriptor owned by the caller.
    let duplicate = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
    if duplicate < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `duplicate` is a fresh descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
}

/// Start Chrome with a private environment rooted at `home` and connect to its CDP pipe.
pub(crate) fn launch(executable: &Path, arguments: &[String], home: &Path) -> std::io::Result<(Chrome, mpsc::UnboundedReceiver<Event>)> {
    let (child_read, parent_write) = pipe_pair()?;
    let (parent_read, child_write) = pipe_pair()?;
    let (child_read, child_write) = (high(child_read)?, high(child_write)?);
    let (read_fd, write_fd) = (child_read.as_raw_fd(), child_write.as_raw_fd());
    let mut command = tokio::process::Command::new(executable);
    command
        .args(arguments)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()))
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .env("XDG_CACHE_HOME", home)
        .env("TMPDIR", home)
        .env("LANG", "C.UTF-8")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // SAFETY: only async-signal-safe calls (dup2) between fork and exec. `dup2` onto 3 and 4
    // clears close-on-exec for the copies; the high originals close on exec.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(read_fd, 3) < 0 || libc::dup2(write_fd, 4) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn()?;
    drop((child_read, child_write));
    let writer = pipe::Sender::from_owned_fd(parent_write)?;
    let mut reader = pipe::Receiver::from_owned_fd(parent_read)?;
    let connection =
        Arc::new(Connection { writer: tokio::sync::Mutex::new(writer), pending: Mutex::new(HashMap::new()), next: AtomicU64::new(1), closed: false.into() });
    let (events, receiver) = mpsc::unbounded_channel();
    let reading = Arc::clone(&connection);
    let task = tokio::spawn(async move {
        let mut buffer: Vec<u8> = Vec::new();
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    buffer.extend_from_slice(&chunk[..n]);
                    while let Some(end) = buffer.iter().position(|b| *b == 0) {
                        let message: Vec<u8> = buffer.drain(..=end).collect();
                        if let Ok(value) = serde_json::from_slice::<Value>(&message[..message.len() - 1]) {
                            reading.dispatch(value, &events);
                        }
                    }
                }
            }
        }
        reading.close();
    });
    let reader = task.abort_handle();
    Ok((Chrome { connection, child: tokio::sync::Mutex::new(Some(child)), reader }, receiver))
}

impl Chrome {
    pub async fn send(&self, session: Option<&str>, method: &str, params: Value) -> CdpResult {
        self.connection.send(session, method, params).await
    }

    /// Close the browser gracefully, then kill it if it does not exit.
    pub async fn close(&self) {
        let _ = tokio::time::timeout(Duration::from_secs(2), self.connection.send(None, "Browser.close", json!({}))).await;
        let mut child = self.child.lock().await;
        if let Some(mut process) = child.take()
            && tokio::time::timeout(Duration::from_secs(3), process.wait()).await.is_err()
        {
            let _ = process.start_kill();
            let _ = process.wait().await;
        }
        self.connection.close();
        self.reader.abort();
    }
}
