use std::sync::Arc;

use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio_util::sync::CancellationToken;

use crate::{api, config::ServerConfig, queue, server};

pub async fn run(cfg: ServerConfig) -> anyhow::Result<()> {
    tracing::info!(
        bind_address = %cfg.bind_addr,
        wiki_count = cfg.wikis.len(),
        "starting wikidesk",
    );

    let bind_addr = cfg.bind_addr;
    let ct = CancellationToken::new();
    let mut background = tokio::task::JoinSet::new();
    let mut router = axum::Router::new();

    for wiki_config in cfg.wikis {
        let wiki_name = wiki_config.name.clone();
        let base_path = wiki_config.base_path();
        let wiki_repo = wiki_config.wiki_repo.clone();
        let (app_state, rx) = queue::AppState::new(wiki_config);
        let state = Arc::new(app_state);

        background.spawn(queue::run_worker(state.clone(), rx));
        background.spawn(queue::run_reaper(state.clone()));

        let service: StreamableHttpService<server::ResearchServer, LocalSessionManager> =
            StreamableHttpService::new(
                {
                    let state = state.clone();
                    move || Ok(server::ResearchServer::new(state.clone()))
                },
                Default::default(),
                StreamableHttpServerConfig::default().with_cancellation_token(ct.child_token()),
            );

        let wiki_router = axum::Router::new()
            .nest_service("/mcp", service)
            .route("/api/research", axum::routing::post(api::research))
            .route("/api/sync", axum::routing::post(api::sync))
            .with_state(state);
        router = router.nest(&base_path, wiki_router);

        tracing::info!(
            wiki = %wiki_name,
            wiki_repo = %wiki_repo.display(),
            base_path = %base_path,
            "mounted wiki",
        );
    }

    let listener = tokio::net::TcpListener::bind(bind_addr).await?;

    tracing::info!(%bind_addr, "listening");

    let serve_future = axum::serve(listener, router).with_graceful_shutdown(async move {
        tokio::signal::ctrl_c().await.unwrap();
        tracing::info!("shutting down");
        ct.cancel();
    });

    tokio::select! {
        result = background.join_next() => {
            match result {
                Some(Ok(())) => anyhow::bail!("background task exited unexpectedly"),
                Some(Err(e)) => anyhow::bail!("background task panicked: {e}"),
                None => anyhow::bail!("all background tasks exited unexpectedly"),
            }
        }
        result = serve_future => result?,
    }

    Ok(())
}
