use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{Mutex, mpsc};
use tokio::time::Instant;

use crate::agent;
use crate::config::AppConfig;

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

#[derive(Debug, Clone)]
struct Task {
    question: String,
    status: TaskStatus,
    completed_at: Option<Instant>,
}

pub struct AppState {
    pub config: AppConfig,
    tasks: Mutex<HashMap<String, Task>>,
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

    pub fn new(config: AppConfig) -> (Self, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel(Self::QUEUE_CAPACITY);
        let state = Self {
            config,
            tasks: Mutex::new(HashMap::new()),
            tx,
        };
        (state, rx)
    }

    pub async fn enqueue(&self, question: String) -> Result<String, QueueFullError> {
        let id = uuid::Uuid::new_v4().to_string();
        self.tx.try_send(id.clone()).map_err(|_| QueueFullError)?;
        let task = Task {
            question,
            status: TaskStatus::Queued,
            completed_at: None,
        };
        self.tasks.lock().await.insert(id.clone(), task);
        Ok(id)
    }

    #[cfg(test)]
    async fn get_task(&self, id: &str) -> Option<Task> {
        self.tasks.lock().await.get(id).cloned()
    }

    pub async fn get_task_status(&self, id: &str) -> Option<TaskStatus> {
        self.tasks.lock().await.get(id).map(|t| t.status.clone())
    }

    async fn start_task(&self, id: &str) -> Option<String> {
        let mut tasks = self.tasks.lock().await;
        let task = tasks.get_mut(id)?;
        task.status = TaskStatus::Running;
        Some(task.question.clone())
    }

    async fn finish_task(&self, id: &str, status: TaskStatus) {
        if let Some(task) = self.tasks.lock().await.get_mut(id) {
            task.status = status;
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
        let question = match state.start_task(&task_id).await {
            Some(q) => q,
            None => continue,
        };
        match agent::run_agent(&state.config, &question).await {
            Ok(answer) => {
                state
                    .finish_task(&task_id, TaskStatus::Done { answer })
                    .await;
            }
            Err(e) => {
                state
                    .finish_task(&task_id, TaskStatus::Failed { error: format!("{e:#}") })
                    .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> AppConfig {
        AppConfig {
            wiki_repo: "/tmp/nonexistent".into(),
            bind_addr: "127.0.0.1:3000".parse().unwrap(),
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
        let task = state.get_task(&id).await.unwrap();
        assert_eq!(task.status, TaskStatus::Queued);
        assert_eq!(task.question, "question");
    }

    #[tokio::test]
    async fn get_task_returns_none_for_unknown_id() {
        let (state, _rx) = test_state();
        assert!(state.get_task("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn complete_task_sets_done_with_result() {
        let (state, _rx) = test_state();
        let id = state.enqueue("q".into()).await.unwrap();
        state.start_task(&id).await;
        state.finish_task(&id, TaskStatus::Done { answer: "the answer".into() }).await;

        let task = state.get_task(&id).await.unwrap();
        assert_eq!(
            task.status,
            TaskStatus::Done {
                answer: "the answer".into(),
            }
        );
    }

    #[tokio::test]
    async fn fail_task_sets_failed_with_error() {
        let (state, _rx) = test_state();
        let id = state.enqueue("q".into()).await.unwrap();
        state.start_task(&id).await;
        state.finish_task(&id, TaskStatus::Failed { error: "agent crashed".into() }).await;

        let task = state.get_task(&id).await.unwrap();
        assert_eq!(
            task.status,
            TaskStatus::Failed {
                error: "agent crashed".into(),
            }
        );
    }

    #[tokio::test]
    async fn sweep_removes_completed_tasks_after_ttl() {
        let (state, _rx) = test_state();
        let id = state.enqueue("q".into()).await.unwrap();
        state.start_task(&id).await;
        state.finish_task(&id, TaskStatus::Done { answer: "done".into() }).await;

        // Zero TTL means all completed tasks are immediately expired
        state.sweep_completed(Duration::ZERO).await;
        assert!(state.get_task(&id).await.is_none());
    }

    #[tokio::test]
    async fn sweep_preserves_active_tasks() {
        let (state, _rx) = test_state();
        let queued_id = state.enqueue("queued".into()).await.unwrap();
        let running_id = state.enqueue("running".into()).await.unwrap();
        state.start_task(&running_id).await;

        state.sweep_completed(Duration::ZERO).await;

        assert!(state.get_task(&queued_id).await.is_some());
        assert!(state.get_task(&running_id).await.is_some());
    }
}
