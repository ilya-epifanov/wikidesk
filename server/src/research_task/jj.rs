pub(super) mod command;
mod workspace;

use std::path::{Path, PathBuf};

use crate::config::GitSyncConfig;
use crate::runner::{ConfiguredAgentRunner, RunnerError};

use super::{OperationContext, PublishedWikiRepo, question_title};
use command::Jj;
use tracing::Instrument;
use workspace::{OwnedWorkspace, is_wikidesk_workspace, remove_dir_if_exists, workspace_root};

pub struct Workflow {
    wiki: String,
    wiki_repo: PathBuf,
    integration_lock: tokio::sync::Mutex<()>,
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
    pub fn new(wiki: String, wiki_repo: PathBuf) -> Self {
        Self {
            wiki,
            wiki_repo,
            integration_lock: tokio::sync::Mutex::new(()),
        }
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

        let op = OperationContext::research(&self.wiki, &self.wiki_repo, task_id);
        let mut tx = JjTransaction::new(&self.wiki_repo);
        let result = self
            .run_research_inner(op, published, agent, prompt, question, &mut tx)
            .await;
        tx.cleanup().await;
        result
    }

    pub async fn sync_remote_once(
        &self,
        published: &PublishedWikiRepo,
        agent: &ConfiguredAgentRunner,
        sync: &GitSyncConfig,
        run_id: &str,
    ) -> Result<(), Error> {
        let _integration = self.integration_lock.lock().await;
        let op = OperationContext::remote_sync(&self.wiki, &self.wiki_repo, run_id, &sync.remote);
        let mut tx = JjTransaction::new(&self.wiki_repo);
        let result = self
            .sync_remote_inner(op, published, agent, sync, &mut tx)
            .await;
        tx.cleanup().await;
        result
    }

    async fn run_research_inner(
        &self,
        op: OperationContext<'_>,
        published: &PublishedWikiRepo,
        agent: &ConfiguredAgentRunner,
        prompt: &str,
        question: &str,
        tx: &mut JjTransaction<'_>,
    ) -> Result<String, Error> {
        let task_id = op.task_id.expect("research context carries task_id");
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
                wiki = %op.wiki,
                repo = %op.repo.display(),
                task_id = %task_id,
                "research produced no repo changes; leaving main unchanged",
            );
            published.prepare().await?;
            return Ok(answer);
        }
        tracing::info!(
            wiki = %op.wiki,
            repo = %op.repo.display(),
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
        let _integration = self.integration_lock.lock().await;
        let main = Jj::new(&self.wiki_repo)
            .commit_id("main", "reading main bookmark")
            .await?;

        if main == research_parent {
            tracing::info!(
                wiki = %op.wiki,
                repo = %op.repo.display(),
                workspace = %research.path.display(),
                task_id = %task_id,
                "publishing research as fast-forward",
            );
            published
                .publish_revision(op, &research.path, "@")
                .instrument(publish_span(op, &research.path))
                .await?;
            return Ok(answer);
        }

        let merge_revs = ["main".to_string(), research_rev];
        let merge_workspace = OwnedWorkspace::merge(&self.wiki_repo, task_id);
        tx.publish_resolved_merge(
            published,
            agent,
            MergeRequest {
                op,
                workspace: merge_workspace.clone(),
                revs: &merge_revs,
                kind: MergeKind::Research { task_id, question },
            },
        )
        .instrument(merge_span(op, &merge_workspace.path))
        .await?;
        Ok(answer)
    }

    async fn sync_remote_inner(
        &self,
        op: OperationContext<'_>,
        published: &PublishedWikiRepo,
        agent: &ConfiguredAgentRunner,
        sync: &GitSyncConfig,
        tx: &mut JjTransaction<'_>,
    ) -> Result<(), Error> {
        self.fetch_and_integrate_remote(op, published, agent, sync, tx)
            .await?;
        tracing::info!(
            wiki = %op.wiki,
            repo = %op.repo.display(),
            remote = %sync.remote,
            run_id = op.run_id.unwrap_or(""),
            "pushing jj main to git remote",
        );
        Jj::new(&self.wiki_repo).git_push_main(sync).await?;
        tracing::info!(
            wiki = %op.wiki,
            repo = %op.repo.display(),
            remote = %sync.remote,
            run_id = op.run_id.unwrap_or(""),
            "pushed jj main to git remote",
        );
        Ok(())
    }

    async fn fetch_and_integrate_remote(
        &self,
        op: OperationContext<'_>,
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
            wiki = %op.wiki,
            repo = %op.repo.display(),
            remote = %sync.remote,
            run_id = op.run_id.unwrap_or(""),
            "fetching jj git remote",
        );
        jj.git_fetch(sync).await?;
        tracing::info!(
            wiki = %op.wiki,
            repo = %op.repo.display(),
            remote = %sync.remote,
            run_id = op.run_id.unwrap_or(""),
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
                    wiki = %op.wiki,
                    repo = %op.repo.display(),
                    remote = %sync.remote,
                    run_id = op.run_id.unwrap_or(""),
                    old_main = current_main.as_deref().unwrap_or("<unknown>"),
                    new_main = %head,
                    "updating main after remote fetch",
                );
                jj.bookmark_set("main", head).await?;
            }
        } else {
            tracing::warn!(
                wiki = %op.wiki,
                repo = %op.repo.display(),
                remote = %sync.remote,
                run_id = op.run_id.unwrap_or(""),
                head_count = main_revs.len(),
                "remote sync found divergent main heads; merging",
            );
            rollback_main(&jj, op, old_main.as_deref()).await;
            self.merge_remote_heads(op, published, agent, sync, tx, &main_revs)
                .await?;
        }
        published.prepare().await?;
        Ok(())
    }

    async fn merge_remote_heads(
        &self,
        op: OperationContext<'_>,
        published: &PublishedWikiRepo,
        agent: &ConfiguredAgentRunner,
        sync: &GitSyncConfig,
        tx: &mut JjTransaction<'_>,
        main_revs: &[String],
    ) -> Result<(), Error> {
        tracing::info!(
            wiki = %op.wiki,
            repo = %op.repo.display(),
            remote = %sync.remote,
            run_id = op.run_id.unwrap_or(""),
            head_count = main_revs.len(),
            "merging remote main divergence",
        );
        let workspace =
            OwnedWorkspace::remote_sync(&self.wiki_repo, op.run_id.unwrap_or("unknown"));
        tx.publish_resolved_merge(
            published,
            agent,
            MergeRequest {
                op,
                workspace: workspace.clone(),
                revs: main_revs,
                kind: MergeKind::RemoteSync {
                    remote: &sync.remote,
                },
            },
        )
        .instrument(merge_span(op, &workspace.path))
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

#[derive(Clone, Copy)]
enum MergeKind<'a> {
    Research { task_id: &'a str, question: &'a str },
    RemoteSync { remote: &'a str },
}

impl MergeKind<'_> {
    fn message(self, notes: Option<&str>) -> String {
        match self {
            Self::Research { task_id, .. } => merge_message(task_id, notes),
            Self::RemoteSync { remote } => remote_sync_message(remote, notes),
        }
    }

    fn prompt(self, conflicts: &str) -> String {
        match self {
            Self::Research { task_id, question } => merge_prompt(task_id, question, conflicts),
            Self::RemoteSync { remote } => remote_sync_prompt(remote, conflicts),
        }
    }
}

struct MergeRequest<'a> {
    op: OperationContext<'a>,
    workspace: OwnedWorkspace,
    revs: &'a [String],
    kind: MergeKind<'a>,
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
        request: MergeRequest<'_>,
    ) -> Result<(), Error> {
        let MergeRequest {
            op,
            workspace,
            revs,
            kind,
        } = request;
        let workspace = self.prepare(workspace).await?;
        let message = kind.message(None);
        tracing::info!("creating jj merge workspace");
        workspace
            .create_merge_revs(self.wiki_repo, revs, &message)
            .await?;
        tracing::info!("created jj merge workspace");
        resolve_conflicts(op, agent, &workspace.path, kind).await?;
        published
            .publish_revision(op, &workspace.path, "@")
            .instrument(publish_span(op, &workspace.path))
            .await
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

fn merge_span(op: OperationContext<'_>, workspace: &Path) -> tracing::Span {
    tracing::info_span!(
        "jj_merge",
        wiki = %op.wiki,
        repo = %op.repo.display(),
        workspace = %workspace.display(),
        task_id = op.task_id.unwrap_or(""),
        run_id = op.run_id.unwrap_or(""),
        remote = op.remote.unwrap_or(""),
    )
}

fn publish_span(op: OperationContext<'_>, workspace: &Path) -> tracing::Span {
    tracing::info_span!(
        "jj_publish",
        wiki = %op.wiki,
        repo = %op.repo.display(),
        workspace = %workspace.display(),
        task_id = op.task_id.unwrap_or(""),
        run_id = op.run_id.unwrap_or(""),
        remote = op.remote.unwrap_or(""),
    )
}

async fn rollback_main(jj: &Jj<'_>, op: OperationContext<'_>, old_main: Option<&str>) {
    let Some(old_main) = old_main else {
        return;
    };
    if let Err(error) = jj.bookmark_set("main", old_main).await {
        tracing::error!(
            wiki = %op.wiki,
            repo = %op.repo.display(),
            run_id = op.run_id.unwrap_or(""),
            remote = op.remote.unwrap_or(""),
            error = %error,
            "failed to roll back main after remote sync failure",
        );
    }
}

async fn resolve_conflicts(
    op: OperationContext<'_>,
    agent: &ConfiguredAgentRunner,
    workspace: &Path,
    kind: MergeKind<'_>,
) -> Result<(), Error> {
    let jj = Jj::new(workspace);
    let conflicts = jj.unresolved_conflicts().await?;
    if conflicts.trim().is_empty() {
        tracing::info!("jj merge has no conflicts");
        return Ok(());
    }
    tracing::warn!(
        conflicts = %conflicts.trim(),
        "jj merge conflicts found; starting resolver",
    );
    let notes = agent
        .run(&kind.prompt(&conflicts), workspace)
        .await?
        .unwrap_or_default();
    tracing::info!(notes_bytes = notes.len(), "jj merge resolver completed");
    jj.snapshot().await?;
    let remaining = jj.unresolved_conflicts().await?;
    if !remaining.trim().is_empty() {
        tracing::warn!(
            wiki = %op.wiki,
            repo = %op.repo.display(),
            workspace = %workspace.display(),
            task_id = op.task_id.unwrap_or(""),
            run_id = op.run_id.unwrap_or(""),
            remote = op.remote.unwrap_or(""),
            conflicts = %remaining.trim(),
            "jj merge conflicts remain after resolver",
        );
        return Err(Error::UnresolvedConflicts { files: remaining });
    }
    if !notes.trim().is_empty() {
        jj.describe(&kind.message(Some(&notes))).await?;
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
