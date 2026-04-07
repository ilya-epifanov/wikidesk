use std::sync::Arc;

use clap::Parser;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio_util::sync::CancellationToken;

mod agent;
mod config;
mod queue;
mod rewrite;
mod server;

#[derive(Parser)]
#[command(name = "research-mcp", about = "MCP server for LLM wiki research")]
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
    let cfg = config::AppConfig::load(&cli.config)?;

    tracing::info!(
        wiki_repo = %cfg.wiki_repo.display(),
        bind_address = %cfg.bind_addr,
        "starting research-mcp",
    );

    let bind_addr = cfg.bind_addr;
    let (app_state, rx) = queue::AppState::new(cfg);
    let state = Arc::new(app_state);

    let worker_state = state.clone();
    let worker_handle = tokio::spawn(async move { queue::run_worker(worker_state, rx).await });

    let reaper_state = state.clone();
    let reaper_handle = tokio::spawn(async move { queue::run_reaper(reaper_state).await });

    let ct = CancellationToken::new();
    let service: StreamableHttpService<server::ResearchServer, LocalSessionManager> =
        StreamableHttpService::new(
            {
                let state = state.clone();
                move || Ok(server::ResearchServer::new(state.clone()))
            },
            Default::default(),
            StreamableHttpServerConfig::default().with_cancellation_token(ct.child_token()),
        );

    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;

    tracing::info!(%bind_addr, "listening");

    let serve_future = axum::serve(listener, router).with_graceful_shutdown(async move {
        tokio::signal::ctrl_c().await.unwrap();
        tracing::info!("shutting down");
        ct.cancel();
    });

    tokio::select! {
        result = worker_handle => {
            match result {
                Ok(()) => anyhow::bail!("worker exited unexpectedly"),
                Err(e) => anyhow::bail!("worker panicked: {e}"),
            }
        }
        result = reaper_handle => {
            match result {
                Ok(()) => anyhow::bail!("reaper exited unexpectedly"),
                Err(e) => anyhow::bail!("reaper panicked: {e}"),
            }
        }
        result = serve_future => result?,
    }

    Ok(())
}
