mod acp;
mod generic;
mod process;
mod stream_json;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

pub use acp::AcpRunner;
pub use generic::GenericRunner;
pub use stream_json::StreamJsonRunner;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunnerType {
    #[default]
    Generic,
    StreamJson,
    Acp,
}

impl RunnerType {
    pub fn requires_prompt_placeholder(self) -> bool {
        match self {
            RunnerType::Generic | RunnerType::StreamJson => true,
            RunnerType::Acp => false,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RunnerError {
    #[error("agent timed out after {secs}s")]
    Timeout { secs: u64 },

    #[error("failed to spawn subprocess")]
    Spawn(#[source] std::io::Error),

    #[error("process exited with code {exit_code}")]
    Exited { exit_code: i32 },

    #[error("I/O error while {op}")]
    Io {
        op: &'static str,
        #[source]
        source: std::io::Error,
    },

    #[error("{kind} failure")]
    Other {
        kind: FailureKind,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::Display)]
#[strum(serialize_all = "snake_case")]
#[non_exhaustive]
pub enum FailureKind {
    Framing,
    Protocol,
}

#[async_trait]
pub trait Runner: Send + Sync {
    async fn run(
        &self,
        command: &[String],
        prompt: &str,
        working_dir: &Path,
        timeout: Duration,
    ) -> Result<Option<String>, RunnerError>;
}

#[derive(Clone)]
pub struct ConfiguredAgentRunner {
    runner: Arc<dyn Runner>,
    command: Vec<String>,
    timeout: Duration,
}

impl ConfiguredAgentRunner {
    pub fn new(runner_type: RunnerType, command: Vec<String>, timeout: Duration) -> Self {
        Self {
            runner: create_runner(runner_type),
            command,
            timeout,
        }
    }

    pub async fn run(
        &self,
        prompt: &str,
        working_dir: &Path,
    ) -> Result<Option<String>, RunnerError> {
        self.runner
            .run(&self.command, prompt, working_dir, self.timeout)
            .await
    }
}

fn create_runner(runner_type: RunnerType) -> Arc<dyn Runner> {
    match runner_type {
        RunnerType::Generic => Arc::new(GenericRunner),
        RunnerType::StreamJson => Arc::new(StreamJsonRunner),
        RunnerType::Acp => Arc::new(AcpRunner),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn generic_runner_captures_stdout() {
        let runner = GenericRunner;
        let result = runner
            .run(
                &["echo".into(), "hello world".into()],
                "unused",
                Path::new("/tmp"),
                Duration::from_secs(5),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.trim(), "hello world");
    }

    #[tokio::test]
    async fn generic_runner_substitutes_prompt() {
        let runner = GenericRunner;
        let result = runner
            .run(
                &["echo".into(), crate::config::PROMPT_PLACEHOLDER.into()],
                "the prompt",
                Path::new("/tmp"),
                Duration::from_secs(5),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.trim(), "the prompt");
    }

    #[tokio::test]
    async fn generic_runner_reports_nonzero_exit() {
        let runner = GenericRunner;
        let result = runner
            .run(
                &["sh".into(), "-c".into(), "exit 42".into()],
                "",
                Path::new("/tmp"),
                Duration::from_secs(5),
            )
            .await;
        match result {
            Err(RunnerError::Exited { exit_code }) => assert_eq!(exit_code, 42),
            other => panic!("expected Exited, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn generic_runner_empty_stdout_returns_none() {
        let runner = GenericRunner;
        let result = runner
            .run(
                &["true".into()],
                "",
                Path::new("/tmp"),
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn generic_runner_timeout() {
        let runner = GenericRunner;
        let result = runner
            .run(
                &["sleep".into(), "10".into()],
                "",
                Path::new("/tmp"),
                Duration::from_millis(100),
            )
            .await;
        assert!(matches!(result, Err(RunnerError::Timeout { .. })));
    }

    #[tokio::test]
    async fn stream_json_runner_parses_result_event() {
        let runner = StreamJsonRunner;
        let script = r#"echo '{"type":"result","result":"the answer"}'"#;
        let result = runner
            .run(
                &["sh".into(), "-c".into(), script.into()],
                "",
                Path::new("/tmp"),
                Duration::from_secs(5),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result, "the answer");
    }

    #[tokio::test]
    async fn stream_json_runner_accumulates_text_deltas() {
        let runner = StreamJsonRunner;
        let script = r#"
echo '{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"hello "}}}'
echo '{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"world"}}}'
"#;
        let result = runner
            .run(
                &["sh".into(), "-c".into(), script.into()],
                "",
                Path::new("/tmp"),
                Duration::from_secs(5),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result, "hello world");
    }

    #[tokio::test]
    async fn stream_json_runner_prefers_result_over_accumulated() {
        let runner = StreamJsonRunner;
        let script = r#"
echo '{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"partial"}}}'
echo '{"type":"result","result":"final answer"}'
"#;
        let result = runner
            .run(
                &["sh".into(), "-c".into(), script.into()],
                "",
                Path::new("/tmp"),
                Duration::from_secs(5),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result, "final answer");
    }

    #[tokio::test]
    async fn stream_json_runner_fails_on_invalid_json() {
        let runner = StreamJsonRunner;
        let result = runner
            .run(
                &["echo".into(), "not json".into()],
                "",
                Path::new("/tmp"),
                Duration::from_secs(5),
            )
            .await;
        assert!(matches!(
            result,
            Err(RunnerError::Other {
                kind: FailureKind::Framing,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn stream_json_runner_returns_none_on_empty_stream() {
        let runner = StreamJsonRunner;
        let result = runner
            .run(
                &["true".into()],
                "",
                Path::new("/tmp"),
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(result, None);
    }

    // Integration tests - require actual Claude installation
    // Run with: cargo test --package wikidesk-server -- --ignored

    #[tokio::test]
    #[ignore = "requires claude CLI"]
    async fn stream_json_runner_with_real_claude() {
        let runner = StreamJsonRunner;
        let result = runner
            .run(
                &[
                    "claude".into(),
                    "-p".into(),
                    crate::config::PROMPT_PLACEHOLDER.into(),
                    "--output-format".into(),
                    "stream-json".into(),
                    "--verbose".into(),
                    "--dangerously-skip-permissions".into(),
                ],
                "What is 2+2? Reply with just the number, nothing else.",
                Path::new("/tmp"),
                Duration::from_secs(60),
            )
            .await;

        match result {
            Ok(Some(answer)) => {
                assert!(
                    answer.contains('4'),
                    "Expected answer to contain '4', got: {answer}"
                );
            }
            Ok(None) => panic!("StreamJson runner returned None"),
            Err(e) => panic!("StreamJson runner failed: {e}"),
        }
    }

    #[tokio::test]
    #[ignore = "requires claude-agent-acp"]
    async fn acp_runner_with_real_claude_agent_acp() {
        let runner = AcpRunner;
        let result = runner
            .run(
                &["claude-agent-acp".into()],
                "What is 2+2? Reply with just the number, nothing else.",
                Path::new("/tmp"),
                Duration::from_secs(60),
            )
            .await;

        match result {
            Ok(Some(answer)) => {
                assert!(
                    answer.contains('4'),
                    "Expected answer to contain '4', got: {answer}"
                );
            }
            Ok(None) => panic!("ACP runner returned None"),
            Err(e) => panic!("ACP runner failed: {e}"),
        }
    }
}
