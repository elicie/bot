#[cfg(feature = "native")]
fn main() -> std::io::Result<()> {
    use std::{
        env, fs,
        io::Write,
        path::{Path, PathBuf},
    };
    fn collect(root: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) -> std::io::Result<()> {
        println!("cargo:rerun-if-changed={}", dir.display());
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let path = entry.path();
            if kind.is_symlink() {
                return Err(std::io::Error::other("Desktop assets must not be symlinks"));
            }
            if kind.is_dir() {
                collect(root, &path, files)?;
            } else if kind.is_file() {
                let name = path.strip_prefix(root).unwrap().to_str().ok_or_else(|| std::io::Error::other("Asset name must be UTF-8"))?.replace('\\', "/");
                files.push((name, path.canonicalize()?));
            }
        }
        Ok(())
    }
    println!("cargo:rerun-if-env-changed=GROK_BOT_BUILD_WEB_ROOT");
    let root = env::var_os("GROK_BOT_BUILD_WEB_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("../../dist"));
    if !root.join("index.html").is_file() {
        return Err(std::io::Error::other("Desktop builds require npm run build first"));
    }
    let mut assets = Vec::new();
    collect(&root, &root, &mut assets)?;
    assets.sort_by(|a, b| a.0.cmp(&b.0));
    let mut output = fs::File::create(PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("web_assets.rs"))?;
    writeln!(output, "static WEB_ASSETS: grok_bot_core::web::EmbeddedWeb = &[")?;
    for (name, path) in assets {
        writeln!(output, "({name:?}, include_bytes!({path:?})),")?;
    }
    writeln!(output, "];")?;
    tauri_build::build();
    Ok(())
}

#[cfg(not(feature = "native"))]
fn main() {}
