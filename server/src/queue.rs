use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{Mutex, mpsc, watch};
use tokio::time::Instant;

use crate::config::AppConfig;
use crate::runner::Runner;

#[derive(Debug, thiserror::Error)]
#[error("research queue is full")]
pub struct QueueFullError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum TaskStatus {
    Queued,
    Running,
    Done { answer: String },
    Failed { error: String },
}

struct Task {
    question: String,
    status: watch::Sender<TaskStatus>,
    completed_at: Option<Instant>,
}

struct TaskStore {
    tasks: Mutex<HashMap<String, Task>>,
}

impl TaskStore {
    fn new() -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
        }
    }

    async fn insert(&self, question: String) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, _rx) = watch::channel(TaskStatus::Queued);
        let task = Task {
            question,
            status: tx,
            completed_at: None,
        };
        self.tasks.lock().await.insert(id.clone(), task);
        id
    }

    async fn get_status(&self, id: &str) -> Option<TaskStatus> {
        self.tasks
            .lock()
            .await
            .get(id)
            .map(|t| t.status.borrow().clone())
    }

    async fn subscribe(&self, id: &str) -> Option<watch::Receiver<TaskStatus>> {
        self.tasks
            .lock()
            .await
            .get(id)
            .map(|t| t.status.subscribe())
    }

    async fn start(&self, id: &str) -> Option<String> {
        let mut tasks = self.tasks.lock().await;
        let task = tasks.get_mut(id)?;
        task.status.send_replace(TaskStatus::Running);
        Some(task.question.clone())
    }

    async fn finish(&self, id: &str, status: TaskStatus) {
        let mut tasks = self.tasks.lock().await;
        if let Some(task) = tasks.get_mut(id) {
            task.status.send_replace(status);
            task.completed_at = Some(Instant::now());
        }
    }

    async fn sweep_completed(&self, ttl: Duration) {
        let now = Instant::now();
        self.tasks.lock().await.retain(|_, task| {
            task.completed_at
                .is_none_or(|completed| now.duration_since(completed) < ttl)
        });
    }
}

pub struct AppState {
    pub config: AppConfig,
    runner: Arc<dyn Runner>,
    research_semaphore: tokio::sync::Semaphore,
    tasks: TaskStore,
    tx: mpsc::Sender<String>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl AppState {
    const QUEUE_CAPACITY: usize = 128;
    const MAX_CONCURRENT_RESEARCH: usize = 2;

    pub fn new(config: AppConfig) -> (Self, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel(Self::QUEUE_CAPACITY);
        let runner = config.create_runner_adapter();
        let state = Self {
            config,
            runner,
            research_semaphore: tokio::sync::Semaphore::new(Self::MAX_CONCURRENT_RESEARCH),
            tasks: TaskStore::new(),
            tx,
        };
        (state, rx)
    }

    pub async fn enqueue(&self, question: String) -> Result<String, QueueFullError> {
        let id = self.tasks.insert(question).await;
        self.tx.try_send(id.clone()).map_err(|_| QueueFullError)?;
        Ok(id)
    }

    pub async fn get_task_status(&self, id: &str) -> Option<TaskStatus> {
        self.tasks.get_status(id).await
    }

    pub async fn submit_and_wait(
        &self,
        question: String,
    ) -> Result<Option<TaskStatus>, QueueFullError> {
        let id = self.enqueue(question).await?;
        Ok(self.wait_for_result(&id).await)
    }

    pub async fn wait_for_result(&self, id: &str) -> Option<TaskStatus> {
        let mut rx = self.tasks.subscribe(id).await?;
        loop {
            {
                let status = rx.borrow_and_update();
                if status.is_terminal() {
                    return Some(status.clone());
                }
            }
            // Sender dropped (task reaped) before completion — shouldn't happen in practice.
            if rx.changed().await.is_err() {
                return None;
            }
        }
    }

    async fn start_task(&self, id: &str) -> Option<String> {
        self.tasks.start(id).await
    }

    async fn finish_task(&self, id: &str, status: TaskStatus) {
        self.tasks.finish(id, status).await;
    }

    async fn sweep_completed(&self, ttl: Duration) {
        self.tasks.sweep_completed(ttl).await;
    }

    async fn execute_queued_task(self: Arc<Self>, task_id: String) {
        let _permit = self.research_semaphore.acquire().await.unwrap();
        let question = match self.start_task(&task_id).await {
            Some(q) => q,
            None => return,
        };
        let prompt = self.config.build_research_prompt(&question);
        let run = self.runner.run(
            &self.config.agent_command,
            &prompt,
            &self.config.wiki_repo,
            self.config.agent_timeout,
        );
        let status = match run.await {
            Ok(Some(answer)) => TaskStatus::Done { answer },
            Ok(None) => TaskStatus::Failed {
                error: "agent produced no output".to_string(),
            },
            Err(e) => TaskStatus::Failed {
                error: format!("{e:#}"),
            },
        };
        self.finish_task(&task_id, status).await;
    }
}

impl TaskStatus {
    fn is_terminal(&self) -> bool {
        matches!(self, TaskStatus::Done { .. } | TaskStatus::Failed { .. })
    }
}

pub async fn run_reaper(state: Arc<AppState>) {
    let ttl = state.config.completed_task_ttl;
    let interval_dur = (ttl / 4).max(Duration::from_secs(15));
    let mut interval = tokio::time::interval_at(Instant::now() + interval_dur, interval_dur);
    loop {
        interval.tick().await;
        state.sweep_completed(ttl).await;
    }
}

pub async fn run_worker(state: Arc<AppState>, mut rx: mpsc::Receiver<String>) {
    while let Some(task_id) = rx.recv().await {
        tokio::spawn(state.clone().execute_queued_task(task_id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> AppConfig {
        AppConfig {
            wiki_repo: "/tmp/nonexistent".into(),
            bind_addr: "127.0.0.1:1238".parse().unwrap(),
            runner: crate::runner::RunnerType::default(),
            agent_command: vec!["echo".into(), "$PROMPT".into()],
            prompt_template_content: String::new(),
            instructions: None,
            research_tool_description: None,
            completed_task_ttl: std::time::Duration::from_secs(900),
            agent_timeout: std::time::Duration::from_secs(1800),
        }
    }

    fn test_state() -> (AppState, mpsc::Receiver<String>) {
        AppState::new(test_config())
    }

    #[tokio::test]
    async fn enqueue_returns_unique_ids() {
        let (state, _rx) = test_state();
        let id1 = state.enqueue("question 1".into()).await.unwrap();
        let id2 = state.enqueue("question 2".into()).await.unwrap();
        assert_ne!(id1, id2);
    }

    #[tokio::test]
    async fn enqueued_task_starts_as_queued() {
        let (state, _rx) = test_state();
        let id = state.enqueue("question".into()).await.unwrap();
        assert_eq!(state.get_task_status(&id).await, Some(TaskStatus::Queued));
    }

    #[tokio::test]
    async fn get_task_status_returns_none_for_unknown_id() {
        let (state, _rx) = test_state();
        assert!(state.get_task_status("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn complete_task_sets_done_with_result() {
        let (state, _rx) = test_state();
        let id = state.enqueue("q".into()).await.unwrap();
        state.start_task(&id).await;
        state
            .finish_task(
                &id,
                TaskStatus::Done {
                    answer: "the answer".into(),
                },
            )
            .await;

        assert_eq!(
            state.get_task_status(&id).await,
            Some(TaskStatus::Done {
                answer: "the answer".into(),
            })
        );
    }

    #[tokio::test]
    async fn fail_task_sets_failed_with_error() {
        let (state, _rx) = test_state();
        let id = state.enqueue("q".into()).await.unwrap();
        state.start_task(&id).await;
        state
            .finish_task(
                &id,
                TaskStatus::Failed {
                    error: "agent crashed".into(),
                },
            )
            .await;

        assert_eq!(
            state.get_task_status(&id).await,
            Some(TaskStatus::Failed {
                error: "agent crashed".into(),
            })
        );
    }

    #[tokio::test]
    async fn sweep_removes_completed_tasks_after_ttl() {
        let (state, _rx) = test_state();
        let id = state.enqueue("q".into()).await.unwrap();
        state.start_task(&id).await;
        state
            .finish_task(
                &id,
                TaskStatus::Done {
                    answer: "done".into(),
                },
            )
            .await;

        state.sweep_completed(Duration::ZERO).await;
        assert!(state.get_task_status(&id).await.is_none());
    }

    #[tokio::test]
    async fn sweep_preserves_active_tasks() {
        let (state, _rx) = test_state();
        let queued_id = state.enqueue("queued".into()).await.unwrap();
        let running_id = state.enqueue("running".into()).await.unwrap();
        state.start_task(&running_id).await;

        state.sweep_completed(Duration::ZERO).await;

        assert!(state.get_task_status(&queued_id).await.is_some());
        assert!(state.get_task_status(&running_id).await.is_some());
    }

    #[tokio::test]
    async fn wait_for_result_receives_notification() {
        let (state, _rx) = test_state();
        let id = state.enqueue("q".into()).await.unwrap();
        state.start_task(&id).await;

        let state_ref = &state;
        let wait = async { state_ref.wait_for_result(&id).await };
        let finish = async {
            state_ref
                .finish_task(
                    &id,
                    TaskStatus::Done {
                        answer: "result".into(),
                    },
                )
                .await;
        };

        let (status, ()) = tokio::join!(wait, finish);
        assert_eq!(
            status,
            Some(TaskStatus::Done {
                answer: "result".into(),
            })
        );
    }
}
