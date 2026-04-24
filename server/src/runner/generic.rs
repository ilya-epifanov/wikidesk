use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use tracing::{error, instrument};

use super::{Runner, RunnerError, build_command, capture_stderr, substitute_prompt};

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
        let mut child = build_command(&args, working_dir)
            .spawn()
            .map_err(RunnerError::Spawn)?;
        let stderr_buf = capture_stderr(&mut child);

        let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(result) => result.map_err(|source| RunnerError::Io {
                op: "waiting for child",
                source,
            })?,
            Err(_) => {
                error!(stderr = %stderr_buf.render(), "agent timed out");
                return Err(RunnerError::Timeout {
                    secs: timeout.as_secs(),
                });
            }
        };

        if !output.status.success() {
            error!(stderr = %stderr_buf.render(), "agent exited with failure");
            let exit_code = output.status.code().unwrap_or(-1);
            return Err(RunnerError::Exited { exit_code });
        }

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        Ok((!stdout.is_empty()).then_some(stdout))
    }
}
