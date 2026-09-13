use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use tracing::Instrument;
use wikidesk_shared::sync::{SyncRequest, SyncResponse, compute_sync};
use wikidesk_shared::{ResearchRequest, ResearchResponse};

use crate::queue::QueueFullError;
use crate::surface::{ResearchSurface, SurfaceError};
use crate::wiki_instance::WikiInstance;

pub(crate) enum ApiError {
    BadRequest(String),
    Busy,
    Internal(String),
}

impl ApiError {
    fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Busy => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::BadRequest(msg) | Self::Internal(msg) => msg,
            Self::Busy => "server busy",
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status(), self.message().to_string()).into_response()
    }
}

impl From<QueueFullError> for ApiError {
    fn from(_: QueueFullError) -> Self {
        Self::Busy
    }
}

impl From<SurfaceError> for ApiError {
    fn from(err: SurfaceError) -> Self {
        match err {
            SurfaceError::QueueFull(_) => Self::Busy,
            SurfaceError::InvalidLocalPath(error) => {
                Self::BadRequest(format!("invalid local_path: {error}"))
            }
            SurfaceError::ResearchFailed(error) => Self::Internal(error),
            other => Self::Internal(other.to_string()),
        }
    }
}

pub async fn research(
    State(state): State<Arc<WikiInstance>>,
    Json(req): Json<ResearchRequest>,
) -> Result<Json<ResearchResponse>, ApiError> {
    let span = tracing::info_span!("http_research", wiki = %state.config.name);
    async move {
        let result = ResearchSurface::new(state)
            .research_and_deliver(req.question, req.local_path)
            .await
            .map(|answer| Json(ResearchResponse { answer }))
            .map_err(ApiError::from);
        log_api_error("research", &result);
        result
    }
    .instrument(span)
    .await
}

pub async fn sync(
    State(state): State<Arc<WikiInstance>>,
    Json(req): Json<SyncRequest>,
) -> Result<Json<SyncResponse>, ApiError> {
    let span = tracing::info_span!("http_sync", wiki = %state.config.name);
    async move {
        let result = sync_inner(state, req).await;
        log_api_error("sync", &result);
        result
    }
    .instrument(span)
    .await
}

async fn sync_inner(
    state: Arc<WikiInstance>,
    req: SyncRequest,
) -> Result<Json<SyncResponse>, ApiError> {
    let published = state
        .prepare_published_for_read()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let wiki_dir = published.wiki_dir().to_path_buf();
    tokio::task::spawn_blocking(move || compute_sync(&wiki_dir, &req.files))
        .await
        .map_err(|e| ApiError::Internal(format!("{e:#}")))?
        .map(Json)
        .map_err(|e| ApiError::Internal(format!("{e:#}")))
}

fn log_api_error<T>(operation: &str, result: &Result<T, ApiError>) {
    let Err(error) = result else {
        return;
    };
    let status = error.status();
    match error {
        ApiError::Internal(_) => tracing::error!(
            operation,
            status = %status,
            error = %error.message(),
            "http api request failed",
        ),
        ApiError::BadRequest(_) | ApiError::Busy => tracing::warn!(
            operation,
            status = %status,
            error = %error.message(),
            "http api request failed",
        ),
    }
}
