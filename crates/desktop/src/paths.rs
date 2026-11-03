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
        if data.components().any(|part| matches!(part, std::path::Component::ParentDir)) {
            bail!("The desktop data path must not contain parent traversal");
        }
        Ok(Self { store: data.join("store"), engine: data.join("engine"), webview: data.join("webview"), data })
    }

    /// Reject overlapping app/source roots BEFORE startup creates engine/profile/lock files.
    /// Resolve a user-selected OS alias while the original HOME is still available; the importer
    /// then pins the resulting real directory and rejects links within the data tree.
    pub fn import_source(&self, source: &Path) -> anyhow::Result<PathBuf> {
        let source = source.canonicalize().context("Cannot resolve the existing web data directory")?;
        if source.starts_with(&self.data) || self.data.starts_with(&source) {
            bail!("Desktop and source web data roots must be separate, non-overlapping directories");
        }
        Ok(source)
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
        assert!(AppPaths::resolve(Some(Path::new("/data/../web"))).is_err());
    }

    #[test]
    fn import_roots_are_checked_without_startup_writes() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let web = root.join("web");
        std::fs::create_dir(&web).unwrap();
        std::fs::create_dir(web.join("backup")).unwrap();
        for data in [&web, &web.join("desktop"), &root] {
            let paths = AppPaths::resolve(Some(data)).unwrap();
            assert!(paths.import_source(&web).is_err());
            assert!(!paths.engine.exists());
        }
        let app = AppPaths::resolve(Some(&root.join("app"))).unwrap();
        assert_eq!(app.import_source(&web).unwrap(), web);
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&web, &alias).unwrap();
        assert!(AppPaths::resolve(Some(&web)).unwrap().import_source(&alias).is_err());
        assert!(!app.data.exists());
    }
}
