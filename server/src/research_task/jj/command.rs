use std::ffi::OsString;
use std::path::Path;

use crate::config::GitSyncConfig;

use super::Error;

pub(in crate::research_task) struct Jj<'a> {
    repo: &'a Path,
}

impl<'a> Jj<'a> {
    pub(in crate::research_task) fn new(repo: &'a Path) -> Self {
        Self { repo }
    }

    pub(in crate::research_task) async fn workspace_names(&self) -> Result<Vec<String>, Error> {
        let output = self
            .run(args(["workspace", "list", "-T", "name ++ \"\\n\""]))
            .await?;
        Ok(output
            .lines()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect())
    }

    pub(in crate::research_task) async fn forget_workspace(&self, name: &str) -> Result<(), Error> {
        self.run(args(["workspace", "forget", name])).await?;
        Ok(())
    }

    pub(in crate::research_task) async fn snapshot(&self) -> Result<(), Error> {
        self.run(args(["util", "snapshot"])).await?;
        Ok(())
    }

    pub(in crate::research_task) async fn diff_summary(&self) -> Result<String, Error> {
        self.run(args(["diff", "--summary", "-r", "@"])).await
    }

    pub(in crate::research_task) async fn unresolved_conflicts(&self) -> Result<String, Error> {
        self.run(args(["resolve", "--list"])).await
    }

    pub(in crate::research_task) async fn describe(&self, message: &str) -> Result<(), Error> {
        self.run(args(["describe", "-m", message])).await?;
        Ok(())
    }

    pub(in crate::research_task) async fn bookmark_set(
        &self,
        name: &str,
        rev: &str,
    ) -> Result<(), Error> {
        self.run(args(["bookmark", "set", name, "-r", rev])).await?;
        Ok(())
    }

    pub(in crate::research_task) async fn commit_id(
        &self,
        rev: &str,
        op: &'static str,
    ) -> Result<String, Error> {
        one_line(self.commit_ids(rev, op).await?, op)
    }

    pub(in crate::research_task) async fn commit_ids(
        &self,
        rev: &str,
        op: &'static str,
    ) -> Result<Vec<String>, Error> {
        let output = self
            .run(args([
                "log",
                "--no-graph",
                "-r",
                rev,
                "-T",
                "commit_id ++ \"\\n\"",
            ]))
            .await?;
        let ids = output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return Err(Error::UnexpectedOutput { op, output });
        }
        Ok(ids)
    }

    pub(in crate::research_task) async fn git_fetch(
        &self,
        sync: &GitSyncConfig,
    ) -> Result<(), Error> {
        self.run_with_git_ssh(
            args(["git", "fetch", "--remote", sync.remote.as_str()]),
            sync.ssh_command.as_deref(),
        )
        .await?;
        self.run(bookmark_track_args(&sync.remote)).await?;
        Ok(())
    }

    pub(in crate::research_task) async fn git_push_main(
        &self,
        sync: &GitSyncConfig,
    ) -> Result<(), Error> {
        self.run_with_git_ssh(
            args([
                "git",
                "push",
                "--remote",
                sync.remote.as_str(),
                "--bookmark",
                "main",
            ]),
            sync.ssh_command.as_deref(),
        )
        .await?;
        Ok(())
    }

    pub(in crate::research_task) async fn run<I>(&self, args: I) -> Result<String, Error>
    where
        I: IntoIterator<Item = OsString>,
    {
        self.run_with_git_ssh(args, None).await
    }

    async fn run_with_git_ssh<I>(&self, args: I, ssh_command: Option<&str>) -> Result<String, Error>
    where
        I: IntoIterator<Item = OsString>,
    {
        let args = args.into_iter().collect::<Vec<_>>();
        let mut command = tokio::process::Command::new("jj");
        command
            .arg("--no-pager")
            .arg("--color")
            .arg("never")
            .arg("-R")
            .arg(self.repo)
            .args(&args);
        if let Some(ssh_command) = ssh_command {
            command.env("GIT_SSH_COMMAND", ssh_command);
        }
        let output = command.output().await.map_err(Error::Spawn)?;

        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
        }

        Err(Error::JjCommand {
            repo: self.repo.to_path_buf(),
            args: display_args(&args),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

pub(in crate::research_task) fn args<const N: usize>(args: [&str; N]) -> [OsString; N] {
    args.map(os)
}

pub(in crate::research_task) fn os(arg: &str) -> OsString {
    OsString::from(arg)
}

fn bookmark_track_args(remote: &str) -> [OsString; 3] {
    [os("bookmark"), os("track"), os(&format!("main@{remote}"))]
}

fn one_line(ids: Vec<String>, op: &'static str) -> Result<String, Error> {
    if ids.len() == 1 {
        return Ok(ids.into_iter().next().unwrap());
    }
    Err(Error::UnexpectedOutput {
        op,
        output: ids.join("\n"),
    })
}

fn display_args(args: &[OsString]) -> String {
    args.iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn bookmark_track_uses_remote_bookmark_syntax() {
        assert_eq!(
            display(&bookmark_track_args("origin")),
            ["bookmark", "track", "main@origin"]
        );
    }
}
