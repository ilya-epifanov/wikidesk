use std::mem;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol as acp;
use async_trait::async_trait;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::{debug, error, instrument};

use super::{AgentProcess, FailureKind, Runner, RunnerError, StderrCapture, substitute_prompt};

#[derive(Debug, Clone, Copy, strum::Display)]
#[strum(serialize_all = "snake_case")]
enum AcpOp {
    Connect,
    Initialize,
    NewSession,
    Prompt,
}

#[derive(Debug, thiserror::Error)]
#[error("ACP {operation} failed: {source}")]
struct AcpError {
    operation: AcpOp,
    #[source]
    source: acp::Error,
}

impl From<AcpError> for RunnerError {
    fn from(err: AcpError) -> Self {
        RunnerError::Other {
            kind: FailureKind::Protocol,
            source: Box::new(err),
        }
    }
}

fn protocol_err(operation: AcpOp, stderr_buf: &StderrCapture, source: acp::Error) -> AcpError {
    error!(stderr = %stderr_buf.render(), %operation, "ACP operation failed");
    AcpError { operation, source }
}

pub struct AcpRunner;

#[async_trait]
impl Runner for AcpRunner {
    #[instrument(skip(self, prompt), fields(command = ?command[0]))]
    async fn run(
        &self,
        command: &[String],
        prompt: &str,
        working_dir: &Path,
        timeout: Duration,
    ) -> Result<Option<String>, RunnerError> {
        let args = substitute_prompt(command, prompt);
        let mut process = AgentProcess::spawn_with_stdin(&args, working_dir)?;

        let stdin = process.child.stdin.take().expect("stdin piped");
        let stdout = process
            .child
            .stdout
            .take()
            .expect("stdout piped by AgentProcess::spawn_with_stdin");
        let stderr_buf = process.stderr.clone();

        let collected_text = Arc::new(Mutex::new(String::new()));
        let transport = acp::ByteStreams::new(stdin.compat_write(), stdout.compat());

        let collected = collected_text.clone();
        let stderr = &stderr_buf;

        let drive = async move {
            acp::Client
                .builder()
                .on_receive_notification(
                    async move |notif: acp::schema::SessionNotification, _cx| {
                        handle_session_update(&collected, notif.update);
                        Ok(())
                    },
                    acp::on_receive_notification!(),
                )
                .on_receive_request(
                    async move |_req: acp::schema::RequestPermissionRequest, responder, _cx| {
                        // DECISION: non-interactive server denies all permission requests.
                        // Cancelled is the standard ACP-sanctioned denial response.
                        responder.respond(acp::schema::RequestPermissionResponse::new(
                            acp::schema::RequestPermissionOutcome::Cancelled,
                        ))
                    },
                    acp::on_receive_request!(),
                )
                .connect_with(
                    transport,
                    async move |connection: acp::ConnectionTo<acp::Agent>| {
                        Ok(run_prompt_turn(&connection, stderr, prompt, working_dir).await)
                    },
                )
                .await
                .map_err(|e| protocol_err(AcpOp::Connect, stderr, e))?
        };

        let outcome = tokio::time::timeout(timeout, drive).await;

        // DECISION: `kill().await` sends SIGKILL and reaps before return.
        // `kill_on_drop` only schedules the signal, leaving a brief zombie window.
        process.kill().await;

        outcome
            .map_err(|_| {
                error!(stderr = %stderr_buf.render(), "ACP agent timed out");
                RunnerError::Timeout {
                    secs: timeout.as_secs(),
                }
            })?
            .map_err(RunnerError::from)?;
        let result = mem::take(&mut *collected_text.lock().unwrap());
        Ok((!result.is_empty()).then_some(result))
    }
}

async fn run_prompt_turn(
    connection: &acp::ConnectionTo<acp::Agent>,
    stderr_buf: &StderrCapture,
    prompt: &str,
    working_dir: &Path,
) -> Result<(), AcpError> {
    connection
        .send_request(
            acp::schema::InitializeRequest::new(acp::schema::ProtocolVersion::V1).client_info(
                acp::schema::Implementation::new("wikidesk", env!("CARGO_PKG_VERSION")),
            ),
        )
        .block_task()
        .await
        .map_err(|e| protocol_err(AcpOp::Initialize, stderr_buf, e))?;
    debug!("ACP initialized");

    let session = connection
        .send_request(acp::schema::NewSessionRequest::new(working_dir))
        .block_task()
        .await
        .map_err(|e| protocol_err(AcpOp::NewSession, stderr_buf, e))?;
    debug!(session_id = ?session.session_id, "session created");

    let prompt_response = connection
        .send_request(acp::schema::PromptRequest::new(
            session.session_id.clone(),
            vec![prompt.into()],
        ))
        .block_task()
        .await
        .map_err(|e| protocol_err(AcpOp::Prompt, stderr_buf, e))?;
    debug!(stop_reason = ?prompt_response.stop_reason, "ACP: prompt completed");
    Ok(())
}

fn handle_session_update(collected: &Mutex<String>, update: acp::schema::SessionUpdate) {
    match update {
        acp::schema::SessionUpdate::AgentMessageChunk(chunk) => {
            if let acp::schema::ContentBlock::Text(text) = chunk.content {
                collected.lock().unwrap().push_str(&text.text);
            }
        }
        acp::schema::SessionUpdate::ToolCall(tc) => {
            debug!(tool = ?tc.title, "tool call started");
        }
        acp::schema::SessionUpdate::ToolCallUpdate(update) => {
            if let Some(status) = &update.fields.status {
                debug!(status = ?status, "tool call update");
            }
        }
        acp::schema::SessionUpdate::Plan(plan) => {
            debug!(entries = plan.entries.len(), "plan update");
        }
        _ => {}
    }
}
