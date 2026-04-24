use std::cell::RefCell;
use std::mem;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use agent_client_protocol::{self as acp, Agent as _};
use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::{debug, error, instrument};

use super::{
    FailureKind, Runner, RunnerError, StderrCapture, build_command, capture_stderr,
    substitute_prompt,
};

fn protocol_err(operation: AcpOperation, stderr_buf: &StderrCapture, err: acp::Error) -> AcpError {
    error!(stderr = %stderr_buf.render(), %operation, "ACP operation failed");
    AcpError::Protocol {
        operation,
        message: err.to_string(),
    }
}

#[derive(Debug, Clone, Copy, strum::Display)]
#[strum(serialize_all = "snake_case")]
enum AcpOperation {
    Initialize,
    NewSession,
    Prompt,
}

#[derive(Debug, Clone, Copy, strum::Display)]
#[strum(serialize_all = "snake_case")]
enum DispatcherStage {
    Send,
    Recv,
}

#[derive(Debug, thiserror::Error)]
enum AcpError {
    #[error("ACP {operation} failed: {message}")]
    Protocol {
        operation: AcpOperation,
        message: String,
    },

    #[error("ACP dispatcher {0} failed")]
    Dispatcher(DispatcherStage),
}

impl From<AcpError> for RunnerError {
    fn from(err: AcpError) -> Self {
        let kind = match &err {
            AcpError::Protocol { .. } => FailureKind::Protocol,
            AcpError::Dispatcher(_) => FailureKind::Internal,
        };
        RunnerError::Other {
            kind,
            source: Box::new(err),
        }
    }
}

// DECISION: a dedicated thread running a current_thread runtime + LocalSet receives
// all ACP jobs. agent-client-protocol uses Rc<RefCell<...>> internally so its futures
// are !Send and must run on a LocalSet. Sharing one thread across requests avoids the
// per-request runtime build and lets multiple concurrent ACP jobs interleave on one
// LocalSet instead of pinning a blocking-pool thread each.
pub struct AcpRunner {
    tx: mpsc::Sender<Job>,
}

struct Job {
    command: Vec<String>,
    prompt: String,
    working_dir: PathBuf,
    timeout: Duration,
    reply: oneshot::Sender<Result<Option<String>, RunnerError>>,
}

impl AcpRunner {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel::<Job>(16);
        std::thread::Builder::new()
            .name("acp-runtime".into())
            .spawn(move || run_dispatcher(rx))
            .expect("spawn acp-runtime thread");
        Self { tx }
    }
}

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
        let (reply, reply_rx) = oneshot::channel();
        self.tx
            .send(Job {
                command: command.to_vec(),
                prompt: prompt.to_string(),
                working_dir: working_dir.to_path_buf(),
                timeout,
                reply,
            })
            .await
            .map_err(|_| AcpError::Dispatcher(DispatcherStage::Send))?;
        reply_rx
            .await
            .map_err(|_| AcpError::Dispatcher(DispatcherStage::Recv))?
    }
}

fn run_dispatcher(mut rx: mpsc::Receiver<Job>) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build acp-runtime tokio runtime");
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        while let Some(job) = rx.recv().await {
            tokio::task::spawn_local(async move {
                let Job {
                    command,
                    prompt,
                    working_dir,
                    timeout,
                    reply,
                } = job;
                let result = run_acp_local(&command, &prompt, &working_dir, timeout).await;
                let _ = reply.send(result);
            });
        }
    });
}

async fn run_acp_local(
    command: &[String],
    prompt: &str,
    working_dir: &Path,
    timeout: Duration,
) -> Result<Option<String>, RunnerError> {
    let args = substitute_prompt(command, prompt);
    let mut child = build_command(&args, working_dir)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(RunnerError::Spawn)?;

    let stdin = child.stdin.take().expect("stdin piped");
    let stdout = child.stdout.take().expect("stdout piped by build_command");
    let stderr_buf = capture_stderr(&mut child);

    let collected_text = Rc::new(RefCell::new(String::new()));

    let client = AcpClient {
        collected_text: collected_text.clone(),
    };

    let (conn, io_task) =
        acp::ClientSideConnection::new(client, stdin.compat_write(), stdout.compat(), |fut| {
            tokio::task::spawn_local(fut);
        });

    tokio::task::spawn_local(io_task);

    let interact = async {
        conn.initialize(
            acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_info(
                acp::Implementation::new("wikidesk", env!("CARGO_PKG_VERSION")),
            ),
        )
        .await
        .map_err(|e| protocol_err(AcpOperation::Initialize, &stderr_buf, e))?;
        debug!("ACP initialized");

        let session = conn
            .new_session(acp::NewSessionRequest::new(working_dir.to_path_buf()))
            .await
            .map_err(|e| protocol_err(AcpOperation::NewSession, &stderr_buf, e))?;
        debug!(session_id = ?session.session_id, "session created");

        let prompt_response = conn
            .prompt(acp::PromptRequest::new(
                session.session_id.clone(),
                vec![prompt.into()],
            ))
            .await
            .map_err(|e| protocol_err(AcpOperation::Prompt, &stderr_buf, e))?;
        debug!(stop_reason = ?prompt_response.stop_reason, "ACP: prompt completed");
        Ok::<(), AcpError>(())
    };

    // DECISION: race the interaction against the timeout inside the LocalSet so a
    // hung agent doesn't leak. A timeout wrapping the outer dispatcher submission
    // would not reach into this future to cancel the subprocess.
    let outcome = tokio::select! {
        r = interact => Some(r),
        _ = tokio::time::sleep(timeout) => {
            error!(stderr = %stderr_buf.render(), "ACP agent timed out");
            None
        }
    };

    drop(conn);
    // DECISION: the ACP agent is a server that doesn't exit after responding; kill
    // it explicitly so the current-thread runtime can unwind promptly. kill_on_drop
    // is a safety net but happens only after this function returns.
    let _ = child.kill().await;

    match outcome {
        Some(Ok(())) => {
            let result = mem::take(&mut *collected_text.borrow_mut());
            Ok((!result.is_empty()).then_some(result))
        }
        Some(Err(e)) => Err(e.into()),
        None => Err(RunnerError::Timeout {
            secs: timeout.as_secs(),
        }),
    }
}

struct AcpClient {
    collected_text: Rc<RefCell<String>>,
}

#[async_trait::async_trait(?Send)]
impl acp::Client for AcpClient {
    async fn request_permission(
        &self,
        _args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        // DECISION: non-interactive server; deny all permission requests.
        Err(acp::Error::method_not_found())
    }

    async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
        match args.update {
            acp::SessionUpdate::AgentMessageChunk(chunk) => {
                if let acp::ContentBlock::Text(text) = chunk.content {
                    self.collected_text.borrow_mut().push_str(&text.text);
                }
            }
            acp::SessionUpdate::ToolCall(tc) => {
                debug!(tool = ?tc.title, "tool call started");
            }
            acp::SessionUpdate::ToolCallUpdate(update) => {
                if let Some(status) = &update.fields.status {
                    debug!(status = ?status, "tool call update");
                }
            }
            acp::SessionUpdate::Plan(plan) => {
                debug!(entries = plan.entries.len(), "plan update");
            }
            _ => {}
        }
        Ok(())
    }
}
