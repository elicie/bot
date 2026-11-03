use anyhow::bail;

/// Print informational commands before touching application data or creating any threads.
pub fn handled() -> anyhow::Result<bool> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.is_empty() {
        return Ok(false);
    }
    if args.len() != 1 {
        bail!("Unknown arguments. Use grok-bot --help.");
    }
    match args[0].to_str() {
        Some("--version") => println!("grok-bot {} (Grok Build {})", env!("CARGO_PKG_VERSION"), grok_bot_engine::host::ENGINE_SOURCE_REVISION),
        Some("--help" | "-h") => println!(
            "Grok Bot {}\n\nUsage: grok-bot [--help | --version | --licenses]\n\nRuns the web UI, HTTP server, and engine in one process.\nOpen http://127.0.0.1:3000 after startup.\n\nEnvironment:\n  HOST                    127.0.0.1 (default), or 0.0.0.0 for trusted LAN access\n  PORT                    3000 (default)\n  GROK_BOT_DATA_DIR       Defaults to $HOME/.local/share/grok-bot-q\n  GROK_BOT_ALLOWED_HOSTS  Additional comma-separated LAN hostnames (no ports)\n  GROK_BOT_WEB_ROOT       Optional directory override containing index.html\n\nThe frontend is embedded at build time; Node.js and dist/ are not needed to run\na release. Managed browser features additionally need system Google Chrome\nand a working host sandbox. LAN access has no login; do not expose it publicly.",
            env!("CARGO_PKG_VERSION")
        ),
        Some("--licenses") => print!(
            "{}\n\n--- Grok Build LICENSE ---\n{}\n\n--- Grok Build third-party notices ---\n{}\n\n--- Grok Build tool-source notices ---\n{}",
            include_str!("../../../engine/NOTICE"),
            include_str!("../../../engine/licenses/GROK-BUILD-LICENSE"),
            include_str!("../../../engine/licenses/GROK-BUILD-THIRD-PARTY-NOTICES"),
            include_str!("../../../engine/licenses/GROK-BUILD-TOOLS-NOTICES.md"),
        ),
        _ => bail!("Unknown argument. Use grok-bot --help."),
    }
    Ok(true)
}
