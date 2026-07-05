use std::collections::VecDeque;
use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

use super::RunnerError;

fn build_command(args: &[&str], working_dir: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(args[0]);
    cmd.args(&args[1..])
        .current_dir(working_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    cmd
}

pub(super) struct AgentProcess {
    pub child: tokio::process::Child,
    pub stderr: StderrCapture,
}

impl AgentProcess {
    pub fn spawn(
        command: &[String],
        prompt: &str,
        working_dir: &Path,
    ) -> Result<Self, RunnerError> {
        let args = substitute_prompt(command, prompt);
        Self::spawn_args(&args, working_dir, false)
    }

    pub fn spawn_with_stdin(
        command: &[String],
        prompt: &str,
        working_dir: &Path,
    ) -> Result<Self, RunnerError> {
        let args = substitute_prompt(command, prompt);
        Self::spawn_args(&args, working_dir, true)
    }

    fn spawn_args(args: &[&str], working_dir: &Path, stdin: bool) -> Result<Self, RunnerError> {
        let mut cmd = build_command(args, working_dir);
        if stdin {
            cmd.stdin(std::process::Stdio::piped());
        }
        let mut child = cmd.spawn().map_err(RunnerError::Spawn)?;
        let stderr = capture_stderr(&mut child);
        Ok(Self { child, stderr })
    }

    pub async fn kill(&mut self) {
        // DECISION: best-effort cleanup; callers already report the original failure.
        let _ = self.child.kill().await;
    }

    pub async fn wait_for_output(
        self,
        timeout: Duration,
    ) -> Result<std::process::Output, RunnerError> {
        let stderr = self.stderr.clone();
        timeout_with_stderr(
            stderr,
            timeout,
            self.child.wait_with_output(),
            "agent timed out",
        )
        .await?
        .map_err(|source| RunnerError::Io {
            op: "waiting for child",
            source,
        })
    }

    pub async fn wait_for_successful_output(
        self,
        timeout: Duration,
    ) -> Result<std::process::Output, RunnerError> {
        let stderr = self.stderr.clone();
        let output = self.wait_for_output(timeout).await?;
        ensure_success(&output.status, &stderr)?;
        Ok(output)
    }

    pub async fn wait_for_success(&mut self) -> Result<(), RunnerError> {
        let status = self.child.wait().await.map_err(|source| RunnerError::Io {
            op: "waiting for child",
            source,
        })?;
        ensure_success(&status, &self.stderr)
    }
}

pub(super) async fn timeout_with_stderr<T>(
    stderr: StderrCapture,
    timeout: Duration,
    future: impl Future<Output = T>,
    message: &'static str,
) -> Result<T, RunnerError> {
    match tokio::time::timeout(timeout, future).await {
        Ok(result) => Ok(result),
        Err(_) => {
            tracing::error!(stderr = %stderr.render(), "{}", message);
            Err(RunnerError::Timeout {
                secs: timeout.as_secs(),
            })
        }
    }
}

fn ensure_success(
    status: &std::process::ExitStatus,
    stderr: &StderrCapture,
) -> Result<(), RunnerError> {
    if status.success() {
        return Ok(());
    }
    tracing::error!(stderr = %stderr.render(), "agent exited with failure");
    Err(RunnerError::Exited {
        exit_code: status.code().unwrap_or(-1),
    })
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

fn substitute_prompt<'a>(command: &'a [String], prompt: &'a str) -> Vec<&'a str> {
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

struct BoundedLineBuffer {
    lines: VecDeque<String>,
    total_bytes: usize,
    dropped_lines: usize,
    max_bytes: usize,
}

impl BoundedLineBuffer {
    fn new(max_bytes: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            total_bytes: 0,
            dropped_lines: 0,
            max_bytes,
        }
    }

    fn push_line(&mut self, line: String) {
        self.total_bytes += line.len();
        self.lines.push_back(line);
        while self.total_bytes > self.max_bytes && self.lines.len() > 1 {
            let dropped = self.lines.pop_front().unwrap();
            self.total_bytes -= dropped.len();
            self.dropped_lines += 1;
        }
    }

    fn render(&self) -> String {
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
}
