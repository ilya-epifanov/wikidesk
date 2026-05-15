use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use tracing::instrument;

use super::{AgentProcess, Runner, RunnerError, substitute_prompt};

pub struct GenericRunner;

#[async_trait]
impl Runner for GenericRunner {
    #[instrument(skip(self, prompt), fields(command = ?command[0]))]
    async fn run(
        &self,
        command: &[String],
        prompt: &str,
        working_dir: &Path,
        timeout: Duration,
    ) -> Result<Option<String>, RunnerError> {
        let args = substitute_prompt(command, prompt);
        let process = AgentProcess::spawn(&args, working_dir)?;
        let stderr_buf = process.stderr.clone();
        let output = process.wait_for_output(timeout).await?;

        if !output.status.success() {
            tracing::error!(stderr = %stderr_buf.render(), "agent exited with failure");
            let exit_code = output.status.code().unwrap_or(-1);
            return Err(RunnerError::Exited { exit_code });
        }

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        Ok((!stdout.is_empty()).then_some(stdout))
    }
}
