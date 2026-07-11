use std::path::{Path, PathBuf};

use directories::ProjectDirs;

use super::Error;
use super::command::{Jj, os};

#[derive(Clone)]
pub(super) struct OwnedWorkspace {
    name: String,
    pub(super) path: PathBuf,
}

impl OwnedWorkspace {
    pub(super) fn research(wiki_repo: &Path, task_id: &str) -> Self {
        Self::new(wiki_repo, "research", task_id)
    }

    pub(super) fn merge(wiki_repo: &Path, task_id: &str) -> Self {
        Self::new(wiki_repo, "merge", task_id)
    }

    pub(super) fn remote_sync(wiki_repo: &Path, run_id: &str) -> Self {
        Self::new(wiki_repo, "remote-sync", run_id)
    }

    fn new(wiki_repo: &Path, kind: &str, task_id: &str) -> Self {
        Self {
            name: format!("wikidesk-{kind}-{task_id}"),
            path: workspace_root(wiki_repo).join(format!("{kind}-{task_id}")),
        }
    }

    pub(super) async fn remove_dir_if_exists(&self) -> Result<(), Error> {
        remove_dir_if_exists(&self.path).await
    }

    pub(super) async fn create_from_main(&self, repo: &Path) -> Result<(), Error> {
        create_parent(&self.path).await?;
        Jj::new(repo)
            .run([
                os("workspace"),
                os("add"),
                os("--name"),
                os(&self.name),
                os("-r"),
                os("main"),
                self.path.as_os_str().to_owned(),
            ])
            .await?;
        Ok(())
    }

    pub(super) async fn create_merge_revs(
        &self,
        repo: &Path,
        revs: &[String],
        message: &str,
    ) -> Result<(), Error> {
        create_parent(&self.path).await?;
        let mut args = vec![os("workspace"), os("add"), os("--name"), os(&self.name)];
        for rev in revs {
            args.push(os("-r"));
            args.push(os(rev));
        }
        args.push(os("-m"));
        args.push(os(message));
        args.push(self.path.as_os_str().to_owned());
        Jj::new(repo).run(args).await?;
        Ok(())
    }

    pub(super) async fn cleanup(&self, repo: &Path) {
        if let Err(err) = Jj::new(repo).forget_workspace(&self.name).await {
            tracing::warn!(workspace = %self.name, error = %err, "failed to forget wikidesk workspace");
        }
        if let Err(err) = remove_dir_if_exists(&self.path).await {
            tracing::warn!(path = %self.path.display(), error = %err, "failed to delete wikidesk workspace");
        }
    }
}

pub(super) fn is_wikidesk_workspace(name: &str) -> bool {
    name.starts_with("wikidesk-research-")
        || name.starts_with("wikidesk-merge-")
        || name.starts_with("wikidesk-remote-sync-")
}

pub(super) async fn create_parent(path: &Path) -> Result<(), Error> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|source| Error::CreateDir {
            path: parent.to_path_buf(),
            source,
        })
}

pub(super) async fn remove_dir_if_exists(path: &Path) -> Result<(), Error> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::RemoveDir {
            path: path.to_path_buf(),
            source,
        }),
    }
}

pub(super) fn workspace_root(wiki_repo: &Path) -> PathBuf {
    let name = wiki_repo
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("wiki");
    workspace_data_root().join(format!("{name}-workspaces"))
}

fn workspace_data_root() -> PathBuf {
    ProjectDirs::from("", "", "wikidesk")
        .map(|dirs| {
            dirs.runtime_dir()
                .unwrap_or_else(|| dirs.cache_dir())
                .join(".workspaces")
        })
        .unwrap_or_else(|| std::env::temp_dir().join("wikidesk").join(".workspaces"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wikidesk_workspace_prefix_includes_remote_sync() {
        assert!(is_wikidesk_workspace("wikidesk-remote-sync-run-1"));
        assert!(!is_wikidesk_workspace("other-remote-sync-run-1"));
    }

    #[test]
    fn workspace_root_uses_per_user_wikidesk_directory() {
        assert_eq!(
            workspace_root(Path::new("/published/wiki-rlhf")),
            workspace_data_root().join("wiki-rlhf-workspaces")
        );
    }
}
