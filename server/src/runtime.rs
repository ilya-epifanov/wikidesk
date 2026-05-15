use std::sync::Arc;

use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio_util::sync::CancellationToken;

use crate::{api, config::AppConfig, queue, server};

pub async fn run(cfg: AppConfig) -> anyhow::Result<()> {
    tracing::info!(
        wiki_repo = %cfg.wiki_repo.display(),
        bind_address = %cfg.bind_addr,
        "starting wikidesk",
    );

    let bind_addr = cfg.bind_addr();
    let (app_state, rx) = queue::AppState::new(cfg);
    let state = Arc::new(app_state);

    let worker_handle = tokio::spawn(queue::run_worker(state.clone(), rx));
    let reaper_handle = tokio::spawn(queue::run_reaper(state.clone()));

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

    let router = axum::Router::new()
        .nest_service("/mcp", service)
        .route("/api/research", axum::routing::post(api::research))
        .route("/api/sync", axum::routing::post(api::sync))
        .with_state(state);
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
