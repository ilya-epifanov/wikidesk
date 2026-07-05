use std::path::{Path, PathBuf};

use crate::config::VcsWorkflow;

use super::jj;
use super::jj::command::{Jj, args};

pub struct PublishedWikiRepo {
    wiki: String,
    repo: PathBuf,
    wiki_dir: PathBuf,
    mode: PublishedMode,
    lock: tokio::sync::Mutex<()>,
}

enum PublishedMode {
    Plain,
    Jj,
}

pub struct PublishedGuard<'a> {
    repo: &'a PublishedWikiRepo,
    _guard: tokio::sync::MutexGuard<'a, ()>,
}

impl PublishedWikiRepo {
    pub(super) fn new(wiki: String, repo: PathBuf, workflow: VcsWorkflow) -> Self {
        let mode = match workflow {
            VcsWorkflow::None => PublishedMode::Plain,
            VcsWorkflow::Jj => PublishedMode::Jj,
        };
        Self {
            wiki,
            wiki_dir: repo.join("wiki"),
            repo,
            mode,
            lock: tokio::sync::Mutex::new(()),
        }
    }

    pub(super) async fn prepare(&self) -> Result<PublishedGuard<'_>, jj::Error> {
        let guard = self.lock.lock().await;
        match self.mode {
            PublishedMode::Plain => {}
            PublishedMode::Jj => prepare_jj_published_workspace(&self.wiki, &self.repo).await?,
        }
        Ok(PublishedGuard {
            repo: self,
            _guard: guard,
        })
    }

    pub(super) async fn publish_revision(
        &self,
        workspace: &Path,
        rev: &str,
    ) -> Result<(), jj::Error> {
        let _guard = self.lock.lock().await;
        match self.mode {
            PublishedMode::Plain => Ok(()),
            PublishedMode::Jj => publish_jj_revision(&self.wiki, &self.repo, workspace, rev).await,
        }
    }

    pub(super) fn wiki_repo(&self) -> &Path {
        &self.repo
    }
}

impl PublishedGuard<'_> {
    pub fn wiki_dir(&self) -> &Path {
        &self.repo.wiki_dir
    }
}

async fn prepare_jj_published_workspace(wiki: &str, repo: &Path) -> Result<(), jj::Error> {
    ensure_published_clean(wiki, repo).await?;
    let jj = Jj::new(repo);
    let parent = jj.commit_id("@-", "reading published parent").await?;
    let main = jj.commit_id("main", "reading main bookmark").await?;
    tracing::debug!(
        wiki = %wiki,
        repo = %repo.display(),
        main = %main,
        published_parent = %parent,
        "checked jj published workspace",
    );
    if parent != main {
        tracing::info!(
            wiki = %wiki,
            repo = %repo.display(),
            from = %parent,
            to = %main,
            "rebasing clean published workspace to main",
        );
        jj.run(args(["rebase", "-r", "@", "-o", "main"])).await?;
        let summary = jj.diff_summary().await?;
        if !summary.trim().is_empty() {
            tracing::warn!(
                wiki = %wiki,
                repo = %repo.display(),
                summary = %summary.trim(),
                "published workspace became dirty after rebase",
            );
            return Err(jj::Error::PublishedDirty {
                repo: repo.to_path_buf(),
                summary,
            });
        }
        tracing::info!(wiki = %wiki, repo = %repo.display(), "rebased clean published workspace to main");
    }
    Ok(())
}

async fn publish_jj_revision(
    wiki: &str,
    repo: &Path,
    workspace: &Path,
    rev: &str,
) -> Result<(), jj::Error> {
    ensure_published_clean(wiki, repo).await?;
    let published_jj = Jj::new(repo);
    let old_main = published_jj
        .commit_id("main", "reading main before publish")
        .await
        .ok();
    let workspace_jj = Jj::new(workspace);
    let new_main = workspace_jj
        .commit_id(rev, "reading revision before publish")
        .await?;
    tracing::info!(
        wiki = %wiki,
        repo = %repo.display(),
        workspace = %workspace.display(),
        old_main = old_main.as_deref().unwrap_or("<unknown>"),
        new_main = %new_main,
        "publishing jj revision",
    );
    workspace_jj.bookmark_set("main", &new_main).await?;
    if let Err(err) = prepare_jj_published_workspace(wiki, repo).await {
        if let Some(old_main) = old_main
            && let Err(rollback) = workspace_jj.bookmark_set("main", &old_main).await
        {
            tracing::error!(error = %rollback, "failed to roll back main after publish failure");
        }
        return Err(err);
    }
    tracing::info!(
        wiki = %wiki,
        repo = %repo.display(),
        workspace = %workspace.display(),
        old_main = old_main.as_deref().unwrap_or("<unknown>"),
        new_main = %new_main,
        "published jj revision",
    );
    Ok(())
}

async fn ensure_published_clean(wiki: &str, repo: &Path) -> Result<(), jj::Error> {
    let jj = Jj::new(repo);
    let summary = jj.diff_summary().await?;
    if !summary.trim().is_empty() {
        tracing::warn!(
            wiki = %wiki,
            repo = %repo.display(),
            summary = %summary.trim(),
            "published wiki workspace has local edits",
        );
        return Err(jj::Error::PublishedDirty {
            repo: repo.to_path_buf(),
            summary,
        });
    }
    Ok(())
}
