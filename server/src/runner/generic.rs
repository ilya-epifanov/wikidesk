use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use tracing::instrument;

use super::process::AgentProcess;
use super::{Runner, RunnerError};

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
        let process = AgentProcess::spawn(command, prompt, working_dir)?;
        let output = process.wait_for_successful_output(timeout).await?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        Ok((!stdout.is_empty()).then_some(stdout))
    }
}
