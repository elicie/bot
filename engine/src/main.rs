use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use xai_grok_shell::agent::config::{AgentMode, Config, RuntimeResolutionContext};

const SOURCE_REVISION: &str = "2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["--version"] {
        println!("grok-bot-engine {} ({SOURCE_REVISION})", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if !args.is_empty() {
        bail!("The application engine accepts only its internal ACP connection.");
    }
    let home = PathBuf::from(std::env::var_os("GROK_HOME").context("Missing application engine home")?);
    let input = std::fs::read_to_string(home.join("config.toml")).context("Cannot read application engine configuration")?;
    let document: toml::Value = toml::from_str(&input).context("Invalid application engine configuration")?;
    let mut config = Config::new_from_toml_cfg(&document).map_err(anyhow::Error::msg)?;
    config.resolve_runtime_fields(&RuntimeResolutionContext {
        raw_config: &document,
        remote_settings: None,
        is_headless: false,
        cli_subagents: Some(false),
        cli_web_search_model: None,
        cli_session_summary_model: Some("local"),
        memory_enabled_override: Some(false),
        disable_web_search: true,
        todo_gate: false,
        laziness_debug_log: None,
        storage_mode: None,
    });
    config.mode = AgentMode::Stdio;
    config.client_version = Some(format!("grok-bot-q {}", env!("CARGO_PKG_VERSION")));
    config.default_model_override = Some("local".into());
    config.default_yolo_mode = false;
    config.default_auto_mode = false;
    xai_grok_shell::agent::app::suppress_otel();
    xai_grok_shell::agent::app::run_stdio_agent(&config, None, None).await
}
