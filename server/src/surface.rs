use std::path::PathBuf;
use std::sync::Arc;

use wikidesk_shared::{FileEntry, SyncResponse, compute_sync};

use crate::delivery::{self, DeliveryError};
use crate::queue::{AppState, QueueFullError, TaskStatus};

#[derive(Clone)]
pub struct ResearchSurface {
    state: Arc<AppState>,
}

impl ResearchSurface {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn submit_research(&self, question: String) -> Result<String, QueueFullError> {
        self.state.enqueue(question).await
    }

    pub async fn research_and_deliver(&self, question: String) -> Result<String, SurfaceError> {
        let status = self.state.submit_and_wait(question).await?;
        self.deliver(status).await
    }

    pub async fn get_status(&self, task_id: &str) -> Option<TaskStatus> {
        self.state.get_task_status(task_id).await
    }

    pub async fn deliver(&self, status: Option<TaskStatus>) -> Result<String, SurfaceError> {
        Ok(delivery::deliver_answer(
            status,
            self.state.config.wiki_repo.clone(),
            self.state.config.client_link_prefix(),
        )
        .await?)
    }

    pub async fn compute_sync(&self, files: Vec<FileEntry>) -> Result<SyncResponse, SurfaceError> {
        let wiki_dir: PathBuf = self.state.config.wiki_dir();
        tokio::task::spawn_blocking(move || compute_sync(&wiki_dir, &files))
            .await
            .map_err(|e| SurfaceError::Internal(format!("{e:#}")))?
            .map_err(|e| SurfaceError::Internal(format!("{e:#}")))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SurfaceError {
    #[error(transparent)]
    QueueFull(#[from] QueueFullError),
    #[error(transparent)]
    Delivery(#[from] DeliveryError),
    #[error("{0}")]
    Internal(String),
}
