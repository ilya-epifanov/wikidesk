pub(super) mod command;
mod workspace;

use std::path::{Path, PathBuf};

use crate::config::GitSyncConfig;
use crate::runner::{ConfiguredAgentRunner, RunnerError};

use super::{PublishedWikiRepo, question_title};
use command::Jj;
use workspace::{OwnedWorkspace, is_wikidesk_workspace, remove_dir_if_exists, workspace_root};

pub struct Workflow {
    wiki_repo: PathBuf,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("wiki repo '{}' is not a jj workspace", .0.display())]
    NotJjWorkspace(PathBuf),
    #[error("failed to create directory '{}'", path.display())]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to remove directory '{}'", path.display())]
    RemoveDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to spawn jj")]
    Spawn(#[source] std::io::Error),
    #[error("jj command failed in '{}': jj {args}\n{stderr}", repo.display())]
    JjCommand {
        repo: PathBuf,
        args: String,
        stderr: String,
    },
    #[error("unexpected jj output while {op}: {output:?}")]
    UnexpectedOutput { op: &'static str, output: String },
    #[error("published wiki workspace '{}' has local edits; commit, move, or discard them first\n{summary}", repo.display())]
    PublishedDirty { repo: PathBuf, summary: String },
    #[error("agent produced no output")]
    AgentNoOutput,
    #[error("merge conflicts remain after resolver\n{files}")]
    UnresolvedConflicts { files: String },
    #[error(transparent)]
    Runner(#[from] RunnerError),
}

impl Error {
    pub(crate) fn is_retryable_remote_sync(&self) -> bool {
        matches!(self, Self::JjCommand { .. })
    }
}

impl Workflow {
    pub fn new(wiki_repo: PathBuf) -> Self {
        Self { wiki_repo }
    }

    pub async fn prepare_startup(&self, published: &PublishedWikiRepo) -> Result<(), Error> {
        if !self.wiki_repo.join(".jj").is_dir() {
            return Err(Error::NotJjWorkspace(self.wiki_repo.clone()));
        }
        Jj::new(&self.wiki_repo)
            .commit_id("main", "checking main bookmark")
            .await?;
        self.cleanup_stale_workspaces().await?;
        published.prepare().await?;
        Ok(())
    }

    pub async fn run_research(
        &self,
        published: &PublishedWikiRepo,
        agent: &ConfiguredAgentRunner,
        prompt: &str,
        task_id: &str,
        question: &str,
    ) -> Result<String, Error> {
        published.prepare().await?;

        let mut tx = JjTransaction::new(&self.wiki_repo);
        let result = self
            .run_research_inner(published, agent, prompt, task_id, question, &mut tx)
            .await;
        tx.cleanup().await;
        result
    }

    pub async fn sync_remote_once(
        &self,
        published: &PublishedWikiRepo,
        agent: &ConfiguredAgentRunner,
        sync: &GitSyncConfig,
    ) -> Result<(), Error> {
        let mut tx = JjTransaction::new(&self.wiki_repo);
        let result = self
            .sync_remote_inner(published, agent, sync, &mut tx)
            .await;
        tx.cleanup().await;
        result
    }

    async fn run_research_inner(
        &self,
        published: &PublishedWikiRepo,
        agent: &ConfiguredAgentRunner,
        prompt: &str,
        task_id: &str,
        question: &str,
        tx: &mut JjTransaction<'_>,
    ) -> Result<String, Error> {
        let research = tx.research_workspace(task_id).await?;

        let answer = agent
            .run(prompt, &research.path)
            .await?
            .ok_or(Error::AgentNoOutput)?;

        let research_jj = Jj::new(&research.path);
        research_jj.snapshot().await?;
        let diff_summary = research_jj.diff_summary().await?;
        if diff_summary.trim().is_empty() {
            tracing::info!(
                repo = %self.wiki_repo.display(),
                task_id = %task_id,
                "research produced no repo changes; leaving main unchanged",
            );
            published.prepare().await?;
            return Ok(answer);
        }
        tracing::info!(
            repo = %self.wiki_repo.display(),
            task_id = %task_id,
            diff_summary = %diff_summary.trim(),
            "research produced repo changes",
        );

        research_jj
            .describe(&research_message(task_id, question))
            .await?;
        let research_rev = research_jj
            .commit_id("@", "reading research commit id")
            .await?;
        let research_parent = research_jj
            .commit_id("@-", "reading research parent id")
            .await?;
        let main = Jj::new(&self.wiki_repo)
            .commit_id("main", "reading main bookmark")
            .await?;

        if main == research_parent {
            published.publish_revision(&research.path, "@").await?;
            return Ok(answer);
        }

        let merge_revs = ["main".to_string(), research_rev];
        tx.publish_resolved_merge(
            published,
            agent,
            OwnedWorkspace::merge(&self.wiki_repo, task_id),
            &merge_revs,
            |conflicts| merge_prompt(task_id, question, conflicts),
            |notes| merge_message(task_id, notes),
        )
        .await?;
        Ok(answer)
    }

    async fn sync_remote_inner(
        &self,
        published: &PublishedWikiRepo,
        agent: &ConfiguredAgentRunner,
        sync: &GitSyncConfig,
        tx: &mut JjTransaction<'_>,
    ) -> Result<(), Error> {
        self.fetch_and_integrate_remote(published, agent, sync, tx)
            .await?;
        tracing::info!(
            repo = %self.wiki_repo.display(),
            remote = %sync.remote,
            "pushing jj main to git remote",
        );
        Jj::new(&self.wiki_repo).git_push_main(sync).await?;
        tracing::info!(
            repo = %self.wiki_repo.display(),
            remote = %sync.remote,
            "pushed jj main to git remote",
        );
        Ok(())
    }

    async fn fetch_and_integrate_remote(
        &self,
        published: &PublishedWikiRepo,
        agent: &ConfiguredAgentRunner,
        sync: &GitSyncConfig,
        tx: &mut JjTransaction<'_>,
    ) -> Result<(), Error> {
        published.prepare().await?;
        let jj = Jj::new(&self.wiki_repo);
        let old_main = jj
            .commit_id("main", "reading main before remote fetch")
            .await
            .ok();
        tracing::info!(
            repo = %self.wiki_repo.display(),
            remote = %sync.remote,
            "fetching jj git remote",
        );
        jj.git_fetch(sync).await?;
        tracing::info!(
            repo = %self.wiki_repo.display(),
            remote = %sync.remote,
            "fetched jj git remote",
        );
        let current_main = jj
            .commit_id("main", "reading main after remote fetch")
            .await
            .ok();
        let main_revs = jj
            .commit_ids(
                &remote_heads_revset(&sync.remote),
                "reading local and remote main heads after fetch",
            )
            .await?;
        if main_revs.len() == 1 {
            let head = &main_revs[0];
            if current_main.as_ref() != Some(head) {
                tracing::info!(
                    repo = %self.wiki_repo.display(),
                    remote = %sync.remote,
                    old_main = current_main.as_deref().unwrap_or("<unknown>"),
                    new_main = %head,
                    "updating main after remote fetch",
                );
                jj.bookmark_set("main", head).await?;
            }
        } else {
            tracing::warn!(
                repo = %self.wiki_repo.display(),
                remote = %sync.remote,
                head_count = main_revs.len(),
                "remote sync found divergent main heads; merging",
            );
            rollback_main(&jj, old_main.as_deref()).await;
            self.merge_remote_heads(published, agent, sync, tx, &main_revs)
                .await?;
        }
        published.prepare().await?;
        Ok(())
    }

    async fn merge_remote_heads(
        &self,
        published: &PublishedWikiRepo,
        agent: &ConfiguredAgentRunner,
        sync: &GitSyncConfig,
        tx: &mut JjTransaction<'_>,
        main_revs: &[String],
    ) -> Result<(), Error> {
        let run_id = uuid::Uuid::new_v4().to_string();
        tracing::info!(
            repo = %self.wiki_repo.display(),
            remote = %sync.remote,
            head_count = main_revs.len(),
            "merging remote main divergence",
        );
        tx.publish_resolved_merge(
            published,
            agent,
            OwnedWorkspace::remote_sync(&self.wiki_repo, &run_id),
            main_revs,
            |conflicts| remote_sync_prompt(&sync.remote, conflicts),
            |notes| remote_sync_message(&sync.remote, notes),
        )
        .await
    }

    async fn cleanup_stale_workspaces(&self) -> Result<(), Error> {
        let jj = Jj::new(&self.wiki_repo);
        for name in jj.workspace_names().await? {
            if is_wikidesk_workspace(&name) {
                jj.forget_workspace(&name).await?;
            }
        }
        remove_dir_if_exists(&workspace_root(&self.wiki_repo)).await
    }
}

struct JjTransaction<'a> {
    wiki_repo: &'a Path,
    cleanup: Vec<OwnedWorkspace>,
}

impl<'a> JjTransaction<'a> {
    fn new(wiki_repo: &'a Path) -> Self {
        Self {
            wiki_repo,
            cleanup: Vec::new(),
        }
    }

    async fn research_workspace(&mut self, task_id: &str) -> Result<OwnedWorkspace, Error> {
        let workspace = self
            .prepare(OwnedWorkspace::research(self.wiki_repo, task_id))
            .await?;
        workspace.create_from_main(self.wiki_repo).await?;
        Ok(workspace)
    }

    async fn publish_resolved_merge(
        &mut self,
        published: &PublishedWikiRepo,
        agent: &ConfiguredAgentRunner,
        workspace: OwnedWorkspace,
        revs: &[String],
        prompt: impl FnOnce(&str) -> String,
        resolved_message: impl Fn(Option<&str>) -> String,
    ) -> Result<(), Error> {
        let workspace = self.prepare(workspace).await?;
        let message = resolved_message(None);
        workspace
            .create_merge_revs(self.wiki_repo, revs, &message)
            .await?;
        resolve_conflicts(agent, &workspace.path, prompt, resolved_message).await?;
        published.publish_revision(&workspace.path, "@").await
    }

    async fn prepare(&mut self, workspace: OwnedWorkspace) -> Result<OwnedWorkspace, Error> {
        self.cleanup.push(workspace.clone());
        workspace.remove_dir_if_exists().await?;
        Ok(workspace)
    }

    async fn cleanup(self) {
        for workspace in self.cleanup.iter().rev() {
            workspace.cleanup(self.wiki_repo).await;
        }
    }
}

async fn rollback_main(jj: &Jj<'_>, old_main: Option<&str>) {
    let Some(old_main) = old_main else {
        return;
    };
    if let Err(error) = jj.bookmark_set("main", old_main).await {
        tracing::error!(error = %error, "failed to roll back main after remote sync failure");
    }
}

async fn resolve_conflicts(
    agent: &ConfiguredAgentRunner,
    workspace: &Path,
    prompt: impl FnOnce(&str) -> String,
    message: impl FnOnce(Option<&str>) -> String,
) -> Result<(), Error> {
    let jj = Jj::new(workspace);
    let conflicts = jj.unresolved_conflicts().await?;
    if conflicts.trim().is_empty() {
        return Ok(());
    }
    let notes = agent
        .run(&prompt(&conflicts), workspace)
        .await?
        .unwrap_or_default();
    jj.snapshot().await?;
    let remaining = jj.unresolved_conflicts().await?;
    if !remaining.trim().is_empty() {
        return Err(Error::UnresolvedConflicts { files: remaining });
    }
    if !notes.trim().is_empty() {
        jj.describe(&message(Some(&notes))).await?;
    }
    Ok(())
}

fn research_message(task_id: &str, question: &str) -> String {
    format!(
        "wikidesk research: {}\n\nTask: {task_id}\n\n{}",
        question_title(question),
        question.trim()
    )
}

fn merge_message(task_id: &str, notes: Option<&str>) -> String {
    match notes.map(str::trim).filter(|notes| !notes.is_empty()) {
        Some(notes) => format!("wikidesk merge: {task_id}\n\nResolution:\n{notes}"),
        None => format!("wikidesk merge: {task_id}"),
    }
}

fn merge_prompt(task_id: &str, question: &str, conflicts: &str) -> String {
    format!(
        "You are resolving a wikidesk jj merge for task {task_id}.\n\n\
The current workspace is a jj merge commit. Resolve all conflicts in this wiki repo. \
Preserve both sides where possible, keep the wiki coherent, and edit only files in this repo. \
When finished, return concise resolution notes.\n\n\
Original research question:\n{question}\n\n\
Conflicted files:\n{conflicts}"
    )
}

fn remote_sync_message(remote: &str, notes: Option<&str>) -> String {
    match notes.map(str::trim).filter(|notes| !notes.is_empty()) {
        Some(notes) => format!("wikidesk remote sync: {remote}/main\n\nResolution:\n{notes}"),
        None => format!("wikidesk remote sync: {remote}/main"),
    }
}

fn remote_sync_prompt(remote: &str, conflicts: &str) -> String {
    format!(
        "You are resolving a wikidesk jj remote sync merge.\n\n\
The current workspace merges local main with fetched {remote}/main. Resolve all conflicts in this wiki repo. \
Preserve both sides where possible, keep the wiki coherent, and edit only files in this repo. \
When finished, return concise resolution notes.\n\n\
Conflicted files:\n{conflicts}"
    )
}

fn remote_heads_revset(remote: &str) -> String {
    format!("heads(main | main@{remote})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_title_compacts_first_non_empty_line() {
        assert_eq!(
            question_title("\n  What   is\tRLHF?  \nextra"),
            "What is RLHF?"
        );
    }

    #[test]
    fn merge_message_includes_resolution_notes_when_present() {
        assert_eq!(
            merge_message("task-1", Some("resolved file conflicts")),
            "wikidesk merge: task-1\n\nResolution:\nresolved file conflicts"
        );
    }

    #[test]
    fn remote_sync_message_includes_resolution_notes_when_present() {
        assert_eq!(
            remote_sync_message("origin", Some("merged upstream wiki edits")),
            "wikidesk remote sync: origin/main\n\nResolution:\nmerged upstream wiki edits"
        );
    }

    #[test]
    fn remote_heads_revset_compares_local_and_remote_main() {
        assert_eq!(remote_heads_revset("origin"), "heads(main | main@origin)");
    }
}
