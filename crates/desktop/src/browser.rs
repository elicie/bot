//! Ordinary user-clicked HTTP(S) links leave the credential-bearing UI and open in the OS browser.
use std::{
    ffi::OsString,
    process::{Command, Stdio},
};

pub struct SystemBrowser {
    environment: Vec<(OsString, OsString)>,
}

impl SystemBrowser {
    /// Capture the desktop session before engine HOME/XDG mutation. Never pass engine settings or
    /// unrelated credentials into a browser-opening subprocess.
    pub fn capture() -> Self {
        let keys = [
            "HOME",
            "PATH",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_CACHE_HOME",
            "XDG_RUNTIME_DIR",
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "DBUS_SESSION_BUS_ADDRESS",
            "XAUTHORITY",
            "LANG",
            "LC_ALL",
            "TMPDIR",
            "XDG_CURRENT_DESKTOP",
            "XDG_SESSION_DESKTOP",
            "XDG_SESSION_TYPE",
            "DESKTOP_SESSION",
        ];
        Self { environment: keys.into_iter().filter_map(|key| std::env::var_os(key).map(|value| (key.into(), value))).collect() }
    }

    pub fn open(&self, address: &str) -> std::io::Result<()> {
        if !external_url(address) {
            return Err(std::io::Error::other("Only ordinary HTTP(S) links can open in the system browser"));
        }
        #[cfg(target_os = "macos")]
        let program = "/usr/bin/open";
        #[cfg(not(target_os = "macos"))]
        let program = "xdg-open";
        let mut child = Command::new(program)
            .arg(address)
            .env_clear()
            .envs(self.environment.iter().cloned())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        // Reap the opener without blocking the native event loop. It has no access to the API token.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}

pub fn external_url(address: &str) -> bool {
    url::Url::parse(address)
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some() && url.username().is_empty() && url.password().is_none())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opener_accepts_only_http_links_without_url_credentials() {
        for url in ["https://example.com/path?q=1#section", "http://127.0.0.1:1234/", "https://example.com/quoted%22value"] {
            assert!(external_url(url), "{url}");
        }
        for url in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "data:text/html,hello",
            "blob:http://127.0.0.1/a",
            "about:blank",
            "https://user:password@example.com",
            "--help",
            "https://",
        ] {
            assert!(!external_url(url), "{url}");
        }
    }
}
