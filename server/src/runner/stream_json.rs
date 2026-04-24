use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::{debug, error, instrument};

use super::{
    FailureKind, Runner, RunnerError, StderrCapture, build_command, capture_stderr,
    substitute_prompt,
};

#[derive(Debug, thiserror::Error)]
enum StreamJsonError {
    #[error("invalid JSON in stream")]
    InvalidJson(#[source] serde_json::Error),

    #[error("agent reported error: {0}")]
    Agent(String),
}

impl From<StreamJsonError> for RunnerError {
    fn from(err: StreamJsonError) -> Self {
        let kind = match &err {
            StreamJsonError::InvalidJson(_) => FailureKind::Framing,
            StreamJsonError::Agent(_) => FailureKind::Protocol,
        };
        RunnerError::Other {
            kind,
            source: Box::new(err),
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Event {
    #[serde(rename = "stream_event")]
    Stream {
        event: StreamPayload,
    },
    System(SystemEvent),
    Result(ResultEvent),
    Error(ErrorEvent),
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
struct StreamPayload {
    delta: Delta,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Delta {
    TextDelta {
        text: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
enum SystemEvent {
    ApiRetry {
        attempt: Option<u64>,
        error: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct ResultEvent {
    result: Option<String>,
}

#[derive(Deserialize)]
struct ErrorEvent {
    error: Option<ErrorDetails>,
    message: Option<String>,
}

#[derive(Deserialize)]
struct ErrorDetails {
    message: Option<String>,
}

impl ErrorEvent {
    fn message(&self) -> &str {
        self.error
            .as_ref()
            .and_then(|e| e.message.as_deref())
            .or(self.message.as_deref())
            .unwrap_or("unknown error")
    }
}

pub struct StreamJsonRunner;

#[async_trait]
impl Runner for StreamJsonRunner {
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
        let stdout = child.stdout.take().expect("stdout piped by build_command");
        let stderr_buf = capture_stderr(&mut child);

        let parse_result =
            tokio::time::timeout(timeout, parse_stream_json(stdout, &mut child, &stderr_buf)).await;

        match parse_result {
            Ok(result) => result,
            Err(_) => {
                error!(stderr = %stderr_buf.render(), "agent timed out");
                Err(RunnerError::Timeout {
                    secs: timeout.as_secs(),
                })
            }
        }
    }
}

async fn parse_stream_json(
    stdout: tokio::process::ChildStdout,
    child: &mut tokio::process::Child,
    stderr_buf: &StderrCapture,
) -> Result<Option<String>, RunnerError> {
    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();
    let mut accumulated_text = String::new();
    let mut final_result: Option<String> = None;

    while let Some(line) = lines.next_line().await.map_err(|source| RunnerError::Io {
        op: "reading stream-json line",
        source,
    })? {
        if line.is_empty() {
            continue;
        }

        let event: Event = serde_json::from_str(&line).map_err(|e| {
            error!(stderr = %stderr_buf.render(), "invalid JSON in stream");
            StreamJsonError::InvalidJson(e)
        })?;

        match event {
            Event::Stream {
                event:
                    StreamPayload {
                        delta: Delta::TextDelta { text },
                    },
            } => {
                accumulated_text.push_str(&text);
            }
            Event::Stream { .. } => {}
            Event::System(SystemEvent::ApiRetry {
                attempt,
                error: err,
            }) => {
                debug!(attempt = ?attempt, error = ?err, "API retry");
            }
            Event::System(SystemEvent::Other) => {
                debug!(raw = %line, "system event");
            }
            Event::Result(ResultEvent { result }) => {
                if let Some(r) = result {
                    final_result = Some(r);
                }
            }
            Event::Error(err) => {
                let message = err.message().to_string();
                error!(stderr = %stderr_buf.render(), message = %message, "agent reported error");
                return Err(StreamJsonError::Agent(message).into());
            }
            Event::Unknown => {
                debug!(raw = %line, "unhandled event");
            }
        }
    }

    let status = child.wait().await.map_err(|source| RunnerError::Io {
        op: "waiting for child",
        source,
    })?;
    if !status.success() {
        error!(stderr = %stderr_buf.render(), "agent exited with failure");
        let exit_code = status.code().unwrap_or(-1);
        return Err(RunnerError::Exited { exit_code });
    }

    Ok(final_result.or((!accumulated_text.is_empty()).then_some(accumulated_text)))
}
