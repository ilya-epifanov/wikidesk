use std::path::PathBuf;
use std::sync::Arc;

use wikidesk_shared::{LocalPathError, validate_local_path};

use crate::queue::{QueueFullError, TaskStatus};
use crate::rewrite;
use crate::wiki_instance::WikiInstance;

#[derive(Clone)]
pub struct ResearchSurface {
    state: Arc<WikiInstance>,
}

impl ResearchSurface {
    pub fn new(state: Arc<WikiInstance>) -> Self {
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
        let task_id = self.state.enqueue(question).await?;
        let status = self.state.wait_for_result(&task_id).await;
        self.deliver_answer(status, local_path).await
    }

    pub async fn poll_result(
        &self,
        task_id: &str,
        local_path: Option<String>,
    ) -> Result<Option<TaskStatus>, SurfaceError> {
        Ok(match self.state.get_task_status(task_id).await {
            Some(TaskStatus::Done { answer }) => Some(TaskStatus::Done {
                answer: self
                    .deliver_answer(
                        Some(TaskStatus::Done { answer }),
                        self.resolve_local_path(local_path)?,
                    )
                    .await?,
            }),
            status => status,
        })
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

    async fn deliver_answer(
        &self,
        status: Option<TaskStatus>,
        local_path: String,
    ) -> Result<String, SurfaceError> {
        let published = self.state.prepare_published_for_read().await?;
        match status.ok_or(SurfaceError::MissingTask)? {
            TaskStatus::Done { answer } => {
                rewrite_answer(answer, published.wiki_dir().to_path_buf(), local_path).await
            }
            TaskStatus::Failed { error } => Err(SurfaceError::ResearchFailed(error)),
            TaskStatus::Queued | TaskStatus::Running => Err(SurfaceError::UnexpectedTaskState),
        }
    }
}

async fn rewrite_answer(
    answer: String,
    wiki_dir: PathBuf,
    link_prefix: String,
) -> Result<String, SurfaceError> {
    tokio::task::spawn_blocking(move || {
        rewrite::rewrite_wikilinks(&answer, &wiki_dir, &link_prefix)
    })
    .await
    .map_err(SurfaceError::RewriteJoin)
}

#[derive(Debug, thiserror::Error)]
pub enum SurfaceError {
    #[error(transparent)]
    QueueFull(#[from] QueueFullError),
    #[error("task disappeared")]
    MissingTask,
    #[error("research failed: {0}")]
    ResearchFailed(String),
    #[error("unexpected task state")]
    UnexpectedTaskState,
    #[error("wikilink rewrite task failed")]
    RewriteJoin(#[source] tokio::task::JoinError),
    #[error("invalid local_path: {0}")]
    InvalidLocalPath(#[from] LocalPathError),
    #[error(transparent)]
    ResearchTask(#[from] crate::research_task::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    fn test_config(wiki_repo: PathBuf, agent_command: Vec<String>) -> crate::config::AppConfig {
        crate::config::test_app_config(wiki_repo, agent_command)
    }

    fn setup_wiki() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let wiki = dir.path().join("wiki/concepts");
        fs::create_dir_all(&wiki).unwrap();
        fs::write(wiki.join("RLHF.md"), "# RLHF").unwrap();
        dir
    }

    #[test]
    fn task_status_serializes_poll_response_shape() {
        assert_eq!(
            serde_json::to_string(&TaskStatus::Done {
                answer: "answer".into()
            })
            .unwrap(),
            r#"{"status":"done","answer":"answer"}"#
        );
        assert_eq!(
            serde_json::to_string(&TaskStatus::Failed {
                error: "boom".into()
            })
            .unwrap(),
            r#"{"status":"failed","error":"boom"}"#
        );
    }

    #[tokio::test]
    async fn poll_result_does_not_validate_local_path_until_done() {
        let dir = setup_wiki();
        let (state, _rx) = WikiInstance::new(test_config(dir.path().into(), vec!["true".into()]));
        let surface = ResearchSurface::new(Arc::new(state));
        let id = surface.submit_research("question".into()).await.unwrap();

        let result = surface
            .poll_result(&id, Some("../bad".into()))
            .await
            .unwrap();

        assert_eq!(result, Some(TaskStatus::Queued));
    }

    #[tokio::test]
    async fn poll_result_delivers_completed_answer() {
        let dir = setup_wiki();
        let (state, rx) = WikiInstance::new(test_config(
            dir.path().into(),
            vec!["printf".into(), "See [[RLHF]].".into()],
        ));
        let state = Arc::new(state);
        let worker = tokio::spawn(crate::wiki_instance::run_worker(state.clone(), rx));
        let surface = ResearchSurface::new(state.clone());
        let id = surface.submit_research("question".into()).await.unwrap();

        tokio::time::timeout(Duration::from_secs(5), state.wait_for_result(&id))
            .await
            .unwrap();

        let result = surface
            .poll_result(&id, Some("wiki-local".into()))
            .await
            .unwrap();

        assert_eq!(
            result,
            Some(TaskStatus::Done {
                answer: "See [RLHF](wiki-local/concepts/RLHF.md).".into(),
            })
        );
        worker.abort();
    }
}
