use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::config::AppConfig;
use crate::queue::{QueueFullError, TaskQueue, TaskStatus};
use crate::remote_sync::RemoteSync;
use crate::research_task::{self, PublishedGuard};

pub struct WikiInstance {
    pub config: AppConfig,
    executor: research_task::Executor,
    research_semaphore: tokio::sync::Semaphore,
    queue: TaskQueue,
    remote_sync: RemoteSync,
}

impl std::fmt::Debug for WikiInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WikiInstance")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl WikiInstance {
    const MAX_CONCURRENT_RESEARCH: usize = 1;

    pub fn new(config: AppConfig) -> (Self, mpsc::Receiver<String>) {
        let (queue, rx) = TaskQueue::new();
        let executor = research_task::Executor::new(&config);
        let remote_sync = RemoteSync::new(config.git_sync.clone());
        (
            Self {
                config,
                executor,
                research_semaphore: tokio::sync::Semaphore::new(Self::MAX_CONCURRENT_RESEARCH),
                queue,
                remote_sync,
            },
            rx,
        )
    }

    pub async fn prepare_executor(&self) -> Result<(), research_task::Error> {
        self.executor.prepare_startup().await
    }

    pub async fn prepare_published_for_read(
        &self,
    ) -> Result<PublishedGuard<'_>, research_task::Error> {
        self.executor.prepare_published_for_read().await
    }

    pub async fn enqueue(&self, question: String) -> Result<String, QueueFullError> {
        let title = research_task::question_title(&question);
        let task_id = self.queue.enqueue(question).await?;
        tracing::info!(
            wiki = %self.config.name,
            task_id = %task_id,
            question = %title,
            "research queued",
        );
        Ok(task_id)
    }

    pub async fn get_task_status(&self, id: &str) -> Option<TaskStatus> {
        self.queue.get_status(id).await
    }

    pub async fn wait_for_result(&self, id: &str) -> Option<TaskStatus> {
        let mut rx = self.queue.subscribe(id).await?;
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

    async fn execute_queued_task(self: Arc<Self>, task_id: String) {
        let _permit = self.research_semaphore.acquire().await.unwrap();
        let question = match self.queue.start(&task_id).await {
            Some(q) => q,
            None => {
                tracing::warn!(
                    wiki = %self.config.name,
                    task_id = %task_id,
                    "queued research task disappeared before start",
                );
                return;
            }
        };
        tracing::info!(wiki = %self.config.name, task_id = %task_id, "research started");
        let started = Instant::now();
        let status = self.run_task(&task_id, &question).await;
        match &status {
            TaskStatus::Done { answer } => tracing::info!(
                wiki = %self.config.name,
                task_id = %task_id,
                duration_ms = started.elapsed().as_millis(),
                answer_bytes = answer.len(),
                "research completed",
            ),
            TaskStatus::Failed { error } => tracing::warn!(
                wiki = %self.config.name,
                task_id = %task_id,
                duration_ms = started.elapsed().as_millis(),
                error = %error,
                "research failed",
            ),
            TaskStatus::Queued | TaskStatus::Running => {}
        }
        let should_sync = matches!(&status, TaskStatus::Done { .. });
        self.queue.finish(&task_id, status).await;
        if should_sync {
            self.remote_sync.request();
        }
    }

    async fn run_task(&self, task_id: &str, question: &str) -> TaskStatus {
        match self.executor.execute(task_id, question).await {
            Ok(answer) => TaskStatus::Done { answer },
            Err(e) => TaskStatus::Failed {
                error: format!("{e:#}"),
            },
        }
    }

    async fn sweep_completed(&self, ttl: Duration) {
        self.queue.sweep_completed(ttl).await;
    }

    async fn run_remote_sync_loop(&self) {
        self.remote_sync
            .run_loop(
                &self.config.name,
                || self.executor.sync_remote_once(),
                research_task::Error::is_retryable_remote_sync,
            )
            .await;
    }
}

pub async fn run_reaper(state: Arc<WikiInstance>) {
    let ttl = state.config.completed_task_ttl;
    let interval_dur = (ttl / 4).max(Duration::from_secs(15));
    let mut interval = tokio::time::interval_at(Instant::now() + interval_dur, interval_dur);
    loop {
        interval.tick().await;
        state.sweep_completed(ttl).await;
    }
}

pub async fn run_remote_sync_loop(state: Arc<WikiInstance>) {
    if state.remote_sync.is_enabled() {
        state.run_remote_sync_loop().await;
    }
}

pub async fn run_worker(state: Arc<WikiInstance>, mut rx: mpsc::Receiver<String>) {
    while let Some(task_id) = rx.recv().await {
        tokio::spawn(state.clone().execute_queued_task(task_id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn test_state() -> (WikiInstance, mpsc::Receiver<String>) {
        WikiInstance::new(crate::config::test_app_config(
            "/tmp/nonexistent".into(),
            vec!["echo".into(), "$PROMPT".into()],
        ))
    }

    #[tokio::test]
    async fn wait_for_result_receives_notification() {
        let (state, _rx) = test_state();
        let id = state.enqueue("q".into()).await.unwrap();
        state.queue.start(&id).await;

        let state_ref = &state;
        let wait = async { state_ref.wait_for_result(&id).await };
        let finish = async {
            state_ref
                .queue
                .finish(
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
