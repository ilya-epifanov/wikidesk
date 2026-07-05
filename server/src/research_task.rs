mod jj;
mod published;

use std::path::Path;

use crate::config::{AppConfig, GitSyncConfig, QUESTION_PLACEHOLDER, VcsWorkflow};
use crate::runner::{ConfiguredAgentRunner, RunnerError};

pub use published::PublishedGuard;
pub(crate) use published::PublishedWikiRepo;

pub struct Executor {
    agent: ConfiguredAgentRunner,
    prompt_template_content: String,
    git_sync: Option<GitSyncConfig>,
    workflow: Workflow,
    published: PublishedWikiRepo,
}

enum Workflow {
    None,
    Jj(jj::Workflow),
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("agent produced no output")]
    AgentNoOutput,
    #[error(transparent)]
    Runner(#[from] RunnerError),
    #[error(transparent)]
    Jj(#[from] jj::Error),
}

impl Error {
    pub(crate) fn is_retryable_remote_sync(&self) -> bool {
        matches!(self, Self::Jj(error) if error.is_retryable_remote_sync())
    }
}

impl Executor {
    pub fn new(config: &AppConfig) -> Self {
        Self {
            agent: ConfiguredAgentRunner::new(
                config.runner,
                config.agent_command.clone(),
                config.agent_timeout,
            ),
            prompt_template_content: config.prompt_template_content.clone(),
            git_sync: config.git_sync.clone(),
            workflow: match config.vcs_workflow {
                VcsWorkflow::None => Workflow::None,
                VcsWorkflow::Jj => Workflow::Jj(jj::Workflow::new(
                    config.name.clone(),
                    config.wiki_repo.clone(),
                )),
            },
            published: PublishedWikiRepo::new(
                config.name.clone(),
                config.wiki_repo.clone(),
                config.vcs_workflow,
            ),
        }
    }

    pub async fn prepare_published_for_read(&self) -> Result<PublishedGuard<'_>, Error> {
        Ok(self.published.prepare().await?)
    }

    pub async fn prepare_startup(&self) -> Result<(), Error> {
        match &self.workflow {
            Workflow::None => Ok(()),
            Workflow::Jj(workflow) => Ok(workflow.prepare_startup(&self.published).await?),
        }
    }

    pub async fn sync_remote_once(&self, run_id: &str) -> Result<(), Error> {
        let (Workflow::Jj(workflow), Some(sync)) = (&self.workflow, &self.git_sync) else {
            return Ok(());
        };
        Ok(workflow
            .sync_remote_once(&self.published, &self.agent, sync, run_id)
            .await?)
    }

    pub async fn execute(&self, task_id: &str, question: &str) -> Result<String, Error> {
        let prompt = self.build_prompt(question);
        match &self.workflow {
            Workflow::None => self.run_direct(&prompt).await,
            Workflow::Jj(workflow) => Ok(workflow
                .run_research(&self.published, &self.agent, &prompt, task_id, question)
                .await?),
        }
    }

    fn build_prompt(&self, question: &str) -> String {
        self.prompt_template_content
            .replace(QUESTION_PLACEHOLDER, question)
    }

    async fn run_direct(&self, prompt: &str) -> Result<String, Error> {
        self.agent
            .run(prompt, self.published.wiki_repo())
            .await?
            .ok_or(Error::AgentNoOutput)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct OperationContext<'a> {
    pub(crate) wiki: &'a str,
    pub(crate) repo: &'a Path,
    pub(crate) task_id: Option<&'a str>,
    pub(crate) run_id: Option<&'a str>,
    pub(crate) remote: Option<&'a str>,
}

impl<'a> OperationContext<'a> {
    pub(crate) fn research(wiki: &'a str, repo: &'a Path, task_id: &'a str) -> Self {
        Self {
            wiki,
            repo,
            task_id: Some(task_id),
            run_id: None,
            remote: None,
        }
    }

    pub(crate) fn remote_sync(
        wiki: &'a str,
        repo: &'a Path,
        run_id: &'a str,
        remote: &'a str,
    ) -> Self {
        Self {
            wiki,
            repo,
            task_id: None,
            run_id: Some(run_id),
            remote: Some(remote),
        }
    }
}

pub(crate) fn question_title(question: &str) -> String {
    let compacted = question
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(question)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut chars = compacted.chars();
    let title = chars.by_ref().take(80).collect::<String>();
    if chars.next().is_some() {
        format!("{title}…")
    } else {
        title
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[tokio::test]
    async fn plain_published_repo_guard_exposes_paths() {
        let published =
            PublishedWikiRepo::new("test".into(), PathBuf::from("/tmp/wiki"), VcsWorkflow::None);
        let guard = published.prepare().await.unwrap();

        assert_eq!(guard.wiki_dir(), Path::new("/tmp/wiki/wiki"));
    }
}
