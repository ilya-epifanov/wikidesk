use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use wikidesk_shared::{ResearchRequest, ResearchResponse, SyncRequest, SyncResponse};

use crate::delivery::DeliveryError;
use crate::queue::{AppState, QueueFullError};
use crate::surface::{ResearchSurface, SurfaceError};

pub(crate) enum ApiError {
    BadRequest(String),
    Busy,
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg).into_response(),
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

impl From<SurfaceError> for ApiError {
    fn from(err: SurfaceError) -> Self {
        match err {
            SurfaceError::QueueFull(_) => Self::Busy,
            SurfaceError::InvalidLocalPath(error) => {
                Self::BadRequest(format!("invalid local_path: {error}"))
            }
            SurfaceError::Delivery(DeliveryError::ResearchFailed(error)) => Self::Internal(error),
            other => Self::Internal(other.to_string()),
        }
    }
}

pub async fn research(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ResearchRequest>,
) -> Result<Json<ResearchResponse>, ApiError> {
    let answer = ResearchSurface::new(state)
        .research_and_deliver(req.question, req.local_path)
        .await?;
    Ok(Json(ResearchResponse { answer }))
}

pub async fn sync(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SyncRequest>,
) -> Result<Json<SyncResponse>, ApiError> {
    ResearchSurface::new(state)
        .compute_sync(req.files)
        .await
        .map(Json)
        .map_err(ApiError::from)
}
