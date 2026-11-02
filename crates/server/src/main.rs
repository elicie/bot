//! Standalone web server: HTTP API, in-process engine, and embedded frontend.

use std::net::Ipv4Addr;
use std::path::PathBuf;

use anyhow::{Context, bail};
use grok_bot_core::app::{Backend, BackendOptions};

mod cli;
include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));

fn frontend() -> anyhow::Result<(Option<PathBuf>, Option<grok_bot_core::web::EmbeddedWeb>)> {
    if let Some(root) = std::env::var_os("GROK_BOT_WEB_ROOT") {
        let root = std::path::absolute(PathBuf::from(root))?;
        if !root.join("index.html").is_file() {
            bail!("GROK_BOT_WEB_ROOT must contain index.html: {}", root.display());
        }
        return Ok((Some(root), None));
    }
    if cfg!(debug_assertions) {
        // Development always sees the latest frontend build, never a stale embedded copy.
        let root = std::path::absolute("dist")?;
        return Ok((root.join("index.html").is_file().then_some(root), None));
    }
    Ok((None, (!WEB_ASSETS.is_empty()).then_some(WEB_ASSETS)))
}

fn ipv4_addresses() -> Vec<Ipv4Addr> {
    let mut addresses = Vec::new();
    // SAFETY: getifaddrs/freeifaddrs pair; entries are read only while the list is alive.
    unsafe {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut list) != 0 {
            return addresses;
        }
        let mut entry = list;
        while !entry.is_null() {
            let item = &*entry;
            if !item.ifa_addr.is_null() && i32::from((*item.ifa_addr).sa_family) == libc::AF_INET && item.ifa_flags & libc::IFF_LOOPBACK as u32 == 0 {
                let address = &*(item.ifa_addr as *const libc::sockaddr_in);
                addresses.push(Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr)));
            }
            entry = item.ifa_next;
        }
        libc::freeifaddrs(list);
    }
    addresses.sort();
    addresses.dedup();
    addresses
}

fn configured_hosts() -> anyhow::Result<Vec<String>> {
    let message = "GROK_BOT_ALLOWED_HOSTS must contain comma-separated hostnames without ports or URLs.";
    let mut hosts = Vec::new();
    for value in std::env::var("GROK_BOT_ALLOWED_HOSTS").unwrap_or_default().split(',').map(str::trim).filter(|v| !v.is_empty()) {
        if value.chars().any(|c| c.is_whitespace() || matches!(c, '/' | '@' | '?' | '#' | '\\' | ':' | '*')) {
            bail!(message);
        }
        hosts.push(value.to_owned());
    }
    Ok(hosts)
}

fn main() -> anyhow::Result<()> {
    if cli::handled()? {
        return Ok(());
    }
    let (web_root, embedded_web) = frontend()?;
    let port: u16 = match std::env::var("PORT") {
        Ok(value) => value.parse().ok().filter(|p| *p >= 1).context("PORT must be an integer between 1 and 65535.")?,
        Err(_) => 3000,
    };
    let host = std::env::var("HOST").unwrap_or_else(|_| "127.0.0.1".into());
    if host != "127.0.0.1" && host != "0.0.0.0" {
        bail!("HOST must be 127.0.0.1 or 0.0.0.0.");
    }
    let configured = configured_hosts()?;
    let data_dir = match std::env::var_os("GROK_BOT_DATA_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".local/share/grok-bot-q"),
    };
    let data_dir = std::path::absolute(data_dir)?;
    let engine_root = data_dir.join("engine");
    let addresses = ipv4_addresses();
    let allowed_hosts = if host == "0.0.0.0" {
        let machine = hostname::get().ok().and_then(|n| n.into_string().ok()).unwrap_or_default();
        let mut hosts = vec!["0.0.0.0".to_owned()];
        if !machine.is_empty() {
            hosts.push(machine.clone());
            hosts.push(format!("{machine}.local"));
        }
        hosts.extend(addresses.iter().map(ToString::to_string));
        hosts.extend(configured);
        hosts
    } else {
        Vec::new()
    };

    // Upstream reads its configuration from the process environment; set it before any thread starts.
    std::fs::create_dir_all(&engine_root).context("Cannot create the engine directory")?;
    // SAFETY: single-threaded at this point; the Tokio runtime is built afterwards.
    unsafe { grok_bot_engine::host::prepare_process_environment(&engine_root) };

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async move {
        let backend =
            Backend::new(BackendOptions { data_dir, engine_root, web_root, embedded_web, allowed_hosts, now: None, scheduler: None, runtime: None }).await?;
        let listener = match tokio::net::TcpListener::bind((host.as_str(), port)).await {
            Ok(listener) => listener,
            Err(error) => bail!("Local server could not start. {error}"),
        };
        println!("Grok Bot is listening at http://{host}:{port}");
        if host == "0.0.0.0" {
            eprintln!("Network access is enabled without login. Use a trusted network.");
            for address in &addresses {
                println!("Network URL: http://{address}:{port}");
            }
        }
        let shutdown = async {
            let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
        };
        backend.serve(listener, shutdown).await
    })
}
