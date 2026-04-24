mod acp;
mod generic;
mod stream_json;

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

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
    Internal,
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

pub(crate) fn create_runner(runner_type: RunnerType) -> Arc<dyn Runner> {
    match runner_type {
        RunnerType::Generic => Arc::new(GenericRunner),
        RunnerType::StreamJson => Arc::new(StreamJsonRunner),
        RunnerType::Acp => Arc::new(AcpRunner::new()),
    }
}

pub(super) fn build_command(args: &[&str], working_dir: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(args[0]);
    cmd.args(&args[1..])
        .current_dir(working_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    cmd
}

#[derive(Clone)]
pub(super) struct StderrCapture(Arc<Mutex<BoundedLineBuffer>>);

impl StderrCapture {
    pub(super) fn render(&self) -> String {
        self.0.lock().unwrap().render()
    }
}

pub(super) fn capture_stderr(child: &mut tokio::process::Child) -> StderrCapture {
    let buf = Arc::new(Mutex::new(BoundedLineBuffer::new(STDERR_MAX_BYTES)));
    if let Some(stderr) = child.stderr.take() {
        let buf = buf.clone();
        tokio::spawn(async move {
            drain_lines(stderr, |line| buf.lock().unwrap().push_line(line)).await;
        });
    }
    StderrCapture(buf)
}

pub(super) fn substitute_prompt<'a>(command: &'a [String], prompt: &'a str) -> Vec<&'a str> {
    command
        .iter()
        .map(|a| {
            if a == crate::config::PROMPT_PLACEHOLDER {
                prompt
            } else {
                a.as_str()
            }
        })
        .collect()
}

const STDERR_MAX_BYTES: usize = 16 * 1024;

pub(super) struct BoundedLineBuffer {
    lines: VecDeque<String>,
    total_bytes: usize,
    dropped_lines: usize,
    max_bytes: usize,
}

impl BoundedLineBuffer {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            total_bytes: 0,
            dropped_lines: 0,
            max_bytes,
        }
    }

    pub fn push_line(&mut self, line: String) {
        self.total_bytes += line.len();
        self.lines.push_back(line);
        while self.total_bytes > self.max_bytes && self.lines.len() > 1 {
            let dropped = self.lines.pop_front().unwrap();
            self.total_bytes -= dropped.len();
            self.dropped_lines += 1;
        }
    }

    pub fn render(&self) -> String {
        let mut result = String::new();
        if self.dropped_lines > 0 {
            result.push_str(&format!(
                "... ({} earlier lines omitted)\n",
                self.dropped_lines
            ));
        }
        for line in &self.lines {
            result.push_str(line);
        }
        result
    }
}

async fn drain_lines<R, F>(reader: R, mut push: F)
where
    R: AsyncRead + Unpin,
    F: FnMut(String),
{
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => push(std::mem::take(&mut line)),
            Err(e) => {
                tracing::warn!(error = %e, "stderr drain failed");
                break;
            }
        }
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

    #[tokio::test]
    async fn bounded_line_buffer_drops_oldest_when_over_cap() {
        let mut buf = BoundedLineBuffer::new(10);
        buf.push_line("aaaa\n".into()); // 5 bytes, total 5
        buf.push_line("bbbb\n".into()); // 5 bytes, total 10
        buf.push_line("cccc\n".into()); // 5 bytes, total 15 -> drops "aaaa\n", total 10
        let rendered = buf.render();
        assert!(rendered.contains("1 earlier lines omitted"));
        assert!(rendered.contains("bbbb\ncccc\n"));
        assert!(!rendered.contains("aaaa"));
    }

    #[tokio::test]
    async fn bounded_line_buffer_keeps_single_oversized_line() {
        let mut buf = BoundedLineBuffer::new(10);
        buf.push_line("xxxxxxxxxxxxxxxx\n".into()); // 17 bytes, over cap but only line
        let rendered = buf.render();
        assert!(!rendered.contains("omitted"));
        assert!(rendered.contains("xxxxxxxxxxxxxxxx"));
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
        let runner = AcpRunner::new();
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
