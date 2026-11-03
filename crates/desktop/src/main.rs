//! Native host only: the exact web HTTP/SSE app and in-process core serve on a private loopback port.
use anyhow::{Context, bail};
use grok_bot_core::{
    app::{Backend, BackendOptions},
    desktop_auth::DesktopToken,
};
use grok_bot_desktop::{
    browser::SystemBrowser,
    import::import_web_data,
    paths::{APP_ID, AppPaths},
};
use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt, path::PathBuf, sync::mpsc, thread};
use tauri::{WebviewUrl, WebviewWindowBuilder};

include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));

#[derive(Default)]
struct Options {
    data: Option<PathBuf>,
    import: Option<PathBuf>,
}
fn options() -> anyhow::Result<Options> {
    let mut options = Options::default();
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--data-dir") => options.data = Some(args.next().context("--data-dir requires a path")?.into()),
            Some("--import-web") => {
                options.import = Some(std::path::absolute(PathBuf::from(args.next().context("--import-web requires a stopped web data directory")?))?)
            }
            Some("--help" | "-h") => {
                println!(
                    "Grok Bot desktop\n  --data-dir <absolute app root>\n  --import-web <stopped web data directory> (first launch only; source preserved)\nUpdates are manual; no background updater is installed."
                );
                std::process::exit(0);
            }
            _ => bail!("Unknown desktop argument"),
        }
    }
    Ok(options)
}

struct CoreHost {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<anyhow::Result<()>>>,
}
impl CoreHost {
    fn close(&mut self) -> anyhow::Result<()> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().map_err(|_| anyhow::anyhow!("Desktop core thread panicked"))??;
        }
        Ok(())
    }
}
impl Drop for CoreHost {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn main() -> anyhow::Result<()> {
    let mut options = options()?;
    let paths = AppPaths::resolve(options.data.as_deref())?;
    if let Some(source) = &mut options.import {
        *source = paths.import_source(source)?;
    }
    grok_bot_files::private_directory(&paths.data)?;
    grok_bot_files::private_directory(&paths.engine)?;
    grok_bot_files::private_directory(&paths.webview)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(paths.data.join("desktop.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock).context("This desktop data directory is already open")?;
    let downloads =
        dirs::download_dir().or_else(|| dirs::home_dir().map(|home| home.join("Downloads"))).context("Cannot determine the user download directory")?;
    let browser = SystemBrowser::capture();
    let context = tauri::generate_context!();
    if context.config().identifier != APP_ID {
        bail!("Desktop application identifier does not match its data path");
    }
    // SAFETY: paths/import source are resolved, and no app/runtime/webview threads have started.
    unsafe { grok_bot_engine::host::prepare_process_environment(&paths.engine) };
    let token = DesktopToken::generate()?;
    let renderer_token = token.as_str().to_owned();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let (ready, startup) = mpsc::sync_channel(1);
    let mut host = CoreHost { stop: Some(stop), thread: None };
    host.thread = Some(thread::Builder::new().name("desktop-core".into()).spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
        runtime.block_on(async move {
            if let Some(source) = options.import {
                import_web_data(&source, &paths.store).await?;
            }
            let backend = Backend::for_desktop(
                BackendOptions {
                    data_dir: paths.store,
                    engine_root: paths.engine,
                    web_root: None,
                    embedded_web: Some(WEB_ASSETS),
                    allowed_hosts: vec![],
                    now: None,
                    scheduler: None,
                    runtime: None,
                },
                token,
            )
            .await?;
            let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
            let origin = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
            ready.send(origin).map_err(|_| anyhow::anyhow!("Desktop window startup was cancelled"))?;
            backend
                .serve(listener, async move {
                    let _ = stopped.await;
                })
                .await
        })
    })?);
    let origin = match startup.recv() {
        Ok(origin) => origin,
        Err(_) => {
            host.close()?;
            bail!("Desktop core did not start");
        }
    };
    let bootstrap = serde_json::json!({ "origin": origin, "token": renderer_token }).to_string();
    let script = format!(
        "if (window.top === window && window.location.origin === {}) {{ Object.defineProperty(window, '__GROK_DESKTOP__', {{value: Object.freeze({bootstrap}), writable: false, configurable: false}}); }}",
        serde_json::to_string(&origin)?
    );
    let origin_url: tauri::Url = origin.parse()?;
    let allowed_origin = origin_url.origin();
    let download_origin = allowed_origin.clone();
    let app = tauri::Builder::default()
        .setup(move |app| {
            WebviewWindowBuilder::new(app, "main", WebviewUrl::External(origin_url))
                .title("Grok Bot")
                .inner_size(1040.0, 760.0)
                .min_inner_size(390.0, 500.0)
                .data_directory(paths.webview)
                // WKWebView has no filesystem profile override on older supported macOS.
                // Keep its browser storage ephemeral rather than share a misleading default store.
                .incognito(cfg!(target_os = "macos"))
                .initialization_script(script)
                // Wry reports iframe navigations here too. Existing sandboxed previews need
                // inert srcdoc/blank documents; neither receives the top-level origin bootstrap.
                .on_navigation(move |url| url.origin() == allowed_origin || matches!(url.as_str(), "about:srcdoc" | "about:blank"))
                .on_new_window(move |url, _| {
                    let _ = browser.open(url.as_str());
                    tauri::webview::NewWindowResponse::Deny
                })
                .on_download(move |_, event| {
                    if let tauri::webview::DownloadEvent::Requested { url, destination } = event {
                        if url.scheme() != "blob" || url.origin() != download_origin {
                            return false;
                        }
                        let Some(name) = destination.file_name().map(|name| name.to_owned()) else {
                            return false;
                        };
                        if std::fs::create_dir_all(&downloads).is_err() {
                            return false;
                        }
                        let mut path = downloads.join(&name);
                        let mut count = 1;
                        while path.exists() {
                            path = downloads.join(format!("{} ({count})", name.to_string_lossy()));
                            count += 1;
                        }
                        *destination = path;
                    }
                    true
                })
                .devtools(cfg!(debug_assertions))
                .build()?;
            Ok(())
        })
        .build(context)?;
    // Unlike App::run, run_return does not terminate the process before core shutdown/flush.
    let code = app.run_return(|_, _| {});
    host.close()?;
    if code != 0 {
        bail!("Desktop event loop exited with code {code}");
    }
    drop(lock);
    Ok(())
}
