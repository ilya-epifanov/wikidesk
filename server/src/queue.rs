use std::collections::HashMap;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{Mutex, mpsc, watch};
use tokio::time::Instant;

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

pub(crate) struct TaskQueue {
    tasks: Mutex<HashMap<String, Task>>,
    tx: mpsc::Sender<String>,
}

impl TaskQueue {
    pub(crate) const CAPACITY: usize = 128;

    pub(crate) fn new() -> (Self, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel(Self::CAPACITY);
        (
            Self {
                tasks: Mutex::new(HashMap::new()),
                tx,
            },
            rx,
        )
    }

    pub(crate) async fn enqueue(&self, question: String) -> Result<String, QueueFullError> {
        let id = self.insert(question).await;
        self.tx.try_send(id.clone()).map_err(|_| QueueFullError)?;
        Ok(id)
    }

    pub(crate) async fn get_status(&self, id: &str) -> Option<TaskStatus> {
        self.tasks
            .lock()
            .await
            .get(id)
            .map(|task| task.status.borrow().clone())
    }

    pub(crate) async fn subscribe(&self, id: &str) -> Option<watch::Receiver<TaskStatus>> {
        self.tasks
            .lock()
            .await
            .get(id)
            .map(|task| task.status.subscribe())
    }

    pub(crate) async fn start(&self, id: &str) -> Option<String> {
        let mut tasks = self.tasks.lock().await;
        let task = tasks.get_mut(id)?;
        task.status.send_replace(TaskStatus::Running);
        Some(task.question.clone())
    }

    pub(crate) async fn finish(&self, id: &str, status: TaskStatus) {
        let mut tasks = self.tasks.lock().await;
        if let Some(task) = tasks.get_mut(id) {
            task.status.send_replace(status);
            task.completed_at = Some(Instant::now());
        }
    }

    pub(crate) async fn sweep_completed(&self, ttl: Duration) {
        let now = Instant::now();
        self.tasks.lock().await.retain(|_, task| {
            task.completed_at
                .is_none_or(|completed| now.duration_since(completed) < ttl)
        });
    }

    async fn insert(&self, question: String) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let (status, _rx) = watch::channel(TaskStatus::Queued);
        self.tasks.lock().await.insert(
            id.clone(),
            Task {
                question,
                status,
                completed_at: None,
            },
        );
        id
    }
}

impl TaskStatus {
    pub(crate) fn is_terminal(&self) -> bool {
        matches!(self, TaskStatus::Done { .. } | TaskStatus::Failed { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_queue() -> (TaskQueue, mpsc::Receiver<String>) {
        TaskQueue::new()
    }

    #[tokio::test]
    async fn enqueue_returns_unique_ids() {
        let (queue, _rx) = test_queue();
        let id1 = queue.enqueue("question 1".into()).await.unwrap();
        let id2 = queue.enqueue("question 2".into()).await.unwrap();
        assert_ne!(id1, id2);
    }

    #[tokio::test]
    async fn enqueued_task_starts_as_queued() {
        let (queue, _rx) = test_queue();
        let id = queue.enqueue("question".into()).await.unwrap();
        assert_eq!(queue.get_status(&id).await, Some(TaskStatus::Queued));
    }

    #[tokio::test]
    async fn get_task_status_returns_none_for_unknown_id() {
        let (queue, _rx) = test_queue();
        assert!(queue.get_status("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn complete_task_sets_done_with_result() {
        let (queue, _rx) = test_queue();
        let id = queue.enqueue("q".into()).await.unwrap();
        queue.start(&id).await;
        queue
            .finish(
                &id,
                TaskStatus::Done {
                    answer: "the answer".into(),
                },
            )
            .await;

        assert_eq!(
            queue.get_status(&id).await,
            Some(TaskStatus::Done {
                answer: "the answer".into(),
            })
        );
    }

    #[tokio::test]
    async fn fail_task_sets_failed_with_error() {
        let (queue, _rx) = test_queue();
        let id = queue.enqueue("q".into()).await.unwrap();
        queue.start(&id).await;
        queue
            .finish(
                &id,
                TaskStatus::Failed {
                    error: "agent crashed".into(),
                },
            )
            .await;

        assert_eq!(
            queue.get_status(&id).await,
            Some(TaskStatus::Failed {
                error: "agent crashed".into(),
            })
        );
    }

    #[tokio::test]
    async fn sweep_removes_completed_tasks_after_ttl() {
        let (queue, _rx) = test_queue();
        let id = queue.enqueue("q".into()).await.unwrap();
        queue.start(&id).await;
        queue
            .finish(
                &id,
                TaskStatus::Done {
                    answer: "done".into(),
                },
            )
            .await;

        queue.sweep_completed(Duration::ZERO).await;
        assert!(queue.get_status(&id).await.is_none());
    }

    #[tokio::test]
    async fn sweep_preserves_active_tasks() {
        let (queue, _rx) = test_queue();
        let queued_id = queue.enqueue("queued".into()).await.unwrap();
        let running_id = queue.enqueue("running".into()).await.unwrap();
        queue.start(&running_id).await;

        queue.sweep_completed(Duration::ZERO).await;

        assert!(queue.get_status(&queued_id).await.is_some());
        assert!(queue.get_status(&running_id).await.is_some());
    }

    #[tokio::test]
    async fn wait_for_result_receives_notification() {
        let (queue, _rx) = test_queue();
        let id = queue.enqueue("q".into()).await.unwrap();
        queue.start(&id).await;
        let mut rx = queue.subscribe(&id).await.unwrap();

        queue
            .finish(
                &id,
                TaskStatus::Done {
                    answer: "result".into(),
                },
            )
            .await;
        rx.changed().await.unwrap();

        assert_eq!(
            rx.borrow().clone(),
            TaskStatus::Done {
                answer: "result".into(),
            }
        );
    }
}
