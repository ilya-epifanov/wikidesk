use clap::Parser;

mod api;
mod config;
mod queue;
mod remote_sync;
mod research_task;
mod rewrite;
mod runner;
mod runtime;
mod server;
mod surface;
mod wiki_instance;

#[derive(Parser)]
#[command(name = "wikidesk", about = "MCP server for LLM wiki research")]
struct Cli {
    /// Path to the configuration file
    #[arg(short, long, default_value = "config.toml")]
    config: std::path::PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let cfg = config::ServerConfig::load(&cli.config)?;

    runtime::run(cfg).await
}
