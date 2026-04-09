use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use wikidesk_shared::{
    FileContent, FileEntry, ResearchRequest, ResearchResponse, SyncRequest, SyncResponse,
    snapshot_dir,
};

use crate::queue::{AppState, QueueFullError, TaskStatus};
use crate::rewrite;

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

pub async fn research(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ResearchRequest>,
) -> Result<Json<ResearchResponse>, ApiError> {
    let id = state.enqueue(req.question).await?;
    let status = state
        .wait_for_result(&id)
        .await
        .ok_or_else(|| ApiError::Internal("task disappeared".into()))?;

    match status {
        TaskStatus::Done { answer } => {
            let wiki_repo = state.config.wiki_repo.clone();
            let wiki_path = req.wiki_path;
            let answer = tokio::task::spawn_blocking(move || {
                rewrite::rewrite_wikilinks(&answer, &wiki_repo, &wiki_path)
            })
            .await
            .map_err(|e| ApiError::Internal(format!("{e:#}")))?;
            Ok(Json(ResearchResponse { answer }))
        }
        TaskStatus::Failed { error } => Err(ApiError::Internal(error)),
        _ => Err(ApiError::Internal("unexpected task state".into())),
    }
}

pub async fn sync(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SyncRequest>,
) -> Result<Json<SyncResponse>, ApiError> {
    let wiki_dir = state.config.wiki_repo.join("wiki");
    tokio::task::spawn_blocking(move || compute_sync(&wiki_dir, &req.files))
        .await
        .map_err(|e| ApiError::Internal(format!("{e:#}")))?
        .map(Json)
        .map_err(|e| ApiError::Internal(format!("{e:#}")))
}

fn compute_sync(
    wiki_dir: &std::path::Path,
    client_files: &[FileEntry],
) -> anyhow::Result<SyncResponse> {
    let server_files = snapshot_dir(wiki_dir)?;

    let client_map: HashMap<&str, &[u8; 32]> = client_files
        .iter()
        .map(|f| (f.path.as_str(), &f.checksum))
        .collect();

    let mut upserts = Vec::new();
    for entry in &server_files {
        let unchanged = client_map
            .get(entry.path.as_str())
            .is_some_and(|c| **c == entry.checksum);
        if !unchanged {
            let content = std::fs::read_to_string(wiki_dir.join(&entry.path))
                .map_err(|e| anyhow::anyhow!("failed to read '{}': {e}", entry.path))?;
            upserts.push(FileContent {
                path: entry.path.clone(),
                content,
            });
        }
    }

    let server_paths: std::collections::HashSet<&str> =
        server_files.iter().map(|f| f.path.as_str()).collect();
    let deletes = client_files
        .iter()
        .filter(|f| !server_paths.contains(f.path.as_str()))
        .map(|f| f.path.clone())
        .collect();

    Ok(SyncResponse { upserts, deletes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_wiki(dir: &std::path::Path) {
        let wiki = dir.join("wiki");
        fs::create_dir_all(wiki.join("concepts")).unwrap();
        fs::write(wiki.join("concepts/RLHF.md"), "# RLHF").unwrap();
        fs::write(wiki.join("topics.md"), "# Topics").unwrap();
    }

    #[test]
    fn sync_new_client_gets_all_files() {
        let dir = tempfile::tempdir().unwrap();
        setup_wiki(dir.path());

        let resp = compute_sync(&dir.path().join("wiki"), &[]).unwrap();
        assert_eq!(resp.deletes.len(), 0);
        assert_eq!(resp.upserts.len(), 2);
    }

    #[test]
    fn sync_up_to_date_client_gets_nothing() {
        let dir = tempfile::tempdir().unwrap();
        setup_wiki(dir.path());

        let client_files = snapshot_dir(&dir.path().join("wiki")).unwrap();
        let resp = compute_sync(&dir.path().join("wiki"), &client_files).unwrap();
        assert!(resp.upserts.is_empty());
        assert!(resp.deletes.is_empty());
    }

    #[test]
    fn sync_detects_deleted_server_file() {
        let dir = tempfile::tempdir().unwrap();
        setup_wiki(dir.path());

        let client_files = vec![FileEntry {
            path: "gone.md".into(),
            checksum: [0xab; 32],
        }];

        let resp = compute_sync(&dir.path().join("wiki"), &client_files).unwrap();
        assert!(resp.deletes.contains(&"gone.md".to_string()));
    }

    #[test]
    fn sync_detects_changed_file() {
        let dir = tempfile::tempdir().unwrap();
        setup_wiki(dir.path());

        let client_files = vec![FileEntry {
            path: "topics.md".into(),
            checksum: [0; 32],
        }];

        let resp = compute_sync(&dir.path().join("wiki"), &client_files).unwrap();
        assert_eq!(resp.upserts.len(), 2); // topics.md (changed) + concepts/RLHF.md (new)
        assert!(resp.upserts.iter().any(|f| f.path == "topics.md"));
    }
}
