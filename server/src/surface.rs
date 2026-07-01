use std::path::PathBuf;
use std::sync::Arc;

use wikidesk_shared::{FileEntry, LocalPathError, SyncResponse, compute_sync, validate_local_path};

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

    pub async fn research_and_deliver(
        &self,
        question: String,
        local_path: Option<String>,
    ) -> Result<String, SurfaceError> {
        let local_path = self.resolve_local_path(local_path)?;
        let status = self.state.submit_and_wait(question).await?;
        self.deliver_to(status, local_path).await
    }

    pub async fn get_status(&self, task_id: &str) -> Option<TaskStatus> {
        self.state.get_task_status(task_id).await
    }

    pub async fn deliver(
        &self,
        status: Option<TaskStatus>,
        local_path: Option<String>,
    ) -> Result<String, SurfaceError> {
        self.deliver_to(status, self.resolve_local_path(local_path)?)
            .await
    }

    fn resolve_local_path(&self, local_path: Option<String>) -> Result<String, SurfaceError> {
        match local_path {
            Some(path) => {
                validate_local_path(&path)?;
                Ok(path)
            }
            None => Ok(self.state.config.derived_wiki_path()),
        }
    }

    async fn deliver_to(
        &self,
        status: Option<TaskStatus>,
        local_path: String,
    ) -> Result<String, SurfaceError> {
        Ok(
            delivery::deliver_answer(status, self.state.config.wiki_repo.clone(), local_path)
                .await?,
        )
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
    #[error("invalid local_path: {0}")]
    InvalidLocalPath(#[from] LocalPathError),
    #[error("{0}")]
    Internal(String),
}
