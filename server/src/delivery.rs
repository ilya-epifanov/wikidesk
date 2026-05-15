use std::path::{Path, PathBuf};

use crate::queue::TaskStatus;
use crate::rewrite;

#[derive(Debug, thiserror::Error)]
pub enum DeliveryError {
    #[error("task disappeared")]
    MissingTask,
    #[error("research failed: {0}")]
    ResearchFailed(String),
    #[error("unexpected task state")]
    UnexpectedState,
    #[error("wikilink rewrite task failed: {0}")]
    RewriteJoin(String),
}

pub async fn deliver_answer(
    status: Option<TaskStatus>,
    wiki_repo: PathBuf,
    link_prefix: String,
) -> Result<String, DeliveryError> {
    match status.ok_or(DeliveryError::MissingTask)? {
        TaskStatus::Done { answer } => rewrite_answer(answer, wiki_repo, link_prefix).await,
        TaskStatus::Failed { error } => Err(DeliveryError::ResearchFailed(error)),
        TaskStatus::Queued | TaskStatus::Running => Err(DeliveryError::UnexpectedState),
    }
}

pub fn render_wikilinks(answer: &str, wiki_repo: &Path, link_prefix: &str) -> String {
    rewrite::rewrite_wikilinks(answer, wiki_repo, link_prefix)
}

async fn rewrite_answer(
    answer: String,
    wiki_repo: PathBuf,
    link_prefix: String,
) -> Result<String, DeliveryError> {
    tokio::task::spawn_blocking(move || render_wikilinks(&answer, &wiki_repo, &link_prefix))
        .await
        .map_err(|e| DeliveryError::RewriteJoin(format!("{e:#}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_wiki() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let wiki = dir.path().join("wiki/concepts");
        fs::create_dir_all(&wiki).unwrap();
        fs::write(wiki.join("RLHF.md"), "# RLHF").unwrap();
        dir
    }

    #[tokio::test]
    async fn delivery_rewrites_successful_answer() {
        let dir = setup_wiki();

        let answer = deliver_answer(
            Some(TaskStatus::Done {
                answer: "See [[RLHF]].".into(),
            }),
            dir.path().to_path_buf(),
            "wiki".into(),
        )
        .await
        .unwrap();

        assert_eq!(answer, "See [RLHF](wiki/concepts/RLHF.md).");
    }

    #[tokio::test]
    async fn delivery_reports_failed_research() {
        let err = deliver_answer(
            Some(TaskStatus::Failed {
                error: "agent failed".into(),
            }),
            PathBuf::from("/tmp/missing"),
            "wiki".into(),
        )
        .await
        .unwrap_err();

        assert!(matches!(err, DeliveryError::ResearchFailed(e) if e == "agent failed"));
    }

    #[tokio::test]
    async fn delivery_rejects_non_terminal_status() {
        let err = deliver_answer(
            Some(TaskStatus::Running),
            PathBuf::from("/tmp/missing"),
            "wiki".into(),
        )
        .await
        .unwrap_err();

        assert!(matches!(err, DeliveryError::UnexpectedState));
    }

    #[tokio::test]
    async fn delivery_reports_missing_task() {
        let err = deliver_answer(None, PathBuf::from("/tmp/missing"), "wiki".into())
            .await
            .unwrap_err();

        assert!(matches!(err, DeliveryError::MissingTask));
    }
}
