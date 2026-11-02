use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

fn collect(root: &Path, dir: &Path, files: &mut Vec<(String, std::path::PathBuf)>) -> io::Result<()> {
    println!("cargo:rerun-if-changed={}", dir.display());
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let path = entry.path();
        // Do not accidentally ship files outside the frontend build directory.
        if kind.is_symlink() {
            return Err(io::Error::other(format!("Frontend assets must not be symlinks: {}", path.display())));
        }
        if kind.is_dir() {
            collect(root, &path, files)?;
        } else if kind.is_file() {
            let relative = path.strip_prefix(root).unwrap().to_str().ok_or_else(|| io::Error::other("Frontend asset name is not UTF-8"))?.replace('\\', "/");
            println!("cargo:rerun-if-changed={}", path.display());
            files.push((relative, path));
        }
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let manifest = std::path::PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest.join("../../dist");
    println!("cargo:rerun-if-changed={}", root.display());
    let mut files = Vec::new();
    if root.join("index.html").is_file() {
        collect(&root, &root, &mut files)?;
        files.sort_by(|a, b| a.0.cmp(&b.0));
    } else if env::var("PROFILE").as_deref() == Ok("release") {
        return Err(io::Error::other("Release builds require dist/index.html. Run npm ci && npm run build, or npm run build:release."));
    }
    let output = std::path::PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("web_assets.rs");
    let mut output = fs::File::create(output)?;
    writeln!(output, "pub static WEB_ASSETS: grok_bot_core::web::EmbeddedWeb = &[")?;
    for (name, path) in files {
        writeln!(output, "    ({name:?}, include_bytes!({:?})),", path.canonicalize()?)?;
    }
    writeln!(output, "];")?;
    Ok(())
}
