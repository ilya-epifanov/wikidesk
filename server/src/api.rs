use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use wikidesk_shared::{ResearchRequest, ResearchResponse, SyncRequest, SyncResponse, compute_sync};

use crate::delivery::{self, DeliveryError};
use crate::queue::{AppState, QueueFullError};

pub(crate) enum ApiError {
    Busy,
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            Self::Busy => (StatusCode::SERVICE_UNAVAILABLE, "server busy").into_response(),
            Self::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response(),
        }
    }
}

impl From<QueueFullError> for ApiError {
    fn from(_: QueueFullError) -> Self {
        Self::Busy
    }
}

impl From<DeliveryError> for ApiError {
    fn from(err: DeliveryError) -> Self {
        match err {
            DeliveryError::ResearchFailed(error) => Self::Internal(error),
            other => Self::Internal(other.to_string()),
        }
    }
}

pub async fn research(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ResearchRequest>,
) -> Result<Json<ResearchResponse>, ApiError> {
    let answer = delivery::deliver_answer(
        state.submit_and_wait(req.question).await?,
        state.config.wiki_repo.clone(),
        req.wiki_path,
    )
    .await?;
    Ok(Json(ResearchResponse { answer }))
}

pub async fn sync(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SyncRequest>,
) -> Result<Json<SyncResponse>, ApiError> {
    let wiki_dir = state.config.wiki_dir();
    tokio::task::spawn_blocking(move || compute_sync(&wiki_dir, &req.files))
        .await
        .map_err(|e| ApiError::Internal(format!("{e:#}")))?
        .map(Json)
        .map_err(|e| ApiError::Internal(format!("{e:#}")))
}
