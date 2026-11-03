//! Resolve OS paths before the engine replaces process-wide HOME/XDG values.
use anyhow::{Context, bail};
use std::path::{Path, PathBuf};

pub const APP_ID: &str = "com.grok-bot.desktop";

pub struct AppPaths {
    pub data: PathBuf,
    pub store: PathBuf,
    pub engine: PathBuf,
    pub webview: PathBuf,
}

impl AppPaths {
    /// Uses the same data-directory convention as Tauri's desktop PathResolver.
    pub fn resolve(data_override: Option<&Path>) -> anyhow::Result<Self> {
        let data = match data_override {
            Some(path) if path.is_absolute() => path.to_path_buf(),
            Some(_) => bail!("--data-dir must be an absolute path"),
            None => dirs::data_dir().context("Cannot determine the OS application data directory")?.join(APP_ID),
        };
        Ok(Self { store: data.join("store"), engine: data.join("engine"), webview: data.join("webview"), data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_paths_are_resolved_before_environment_changes() {
        let paths = AppPaths::resolve(Some(Path::new("/Users/test/Application Support/test-app"))).unwrap();
        assert_eq!(paths.engine, paths.data.join("engine"));
        assert_eq!(paths.webview, paths.data.join("webview"));
        assert!(AppPaths::resolve(Some(Path::new("relative"))).is_err());
    }
}
