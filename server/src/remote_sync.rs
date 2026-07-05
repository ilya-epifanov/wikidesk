use std::fmt::Display;
use std::future::Future;

use tokio::sync::{Mutex, Notify};
use tokio::time::Instant;

use crate::config::GitSyncConfig;

pub(crate) struct RemoteSync {
    config: Option<GitSyncConfig>,
    lock: Mutex<()>,
    trigger: Notify,
}

impl RemoteSync {
    pub(crate) fn new(config: Option<GitSyncConfig>) -> Self {
        Self {
            config,
            lock: Mutex::new(()),
            trigger: Notify::new(),
        }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.config.is_some()
    }

    pub(crate) fn request(&self) {
        if self.is_enabled() {
            self.trigger.notify_one();
        }
    }

    pub(crate) async fn run_loop<E, F, Fut>(
        &self,
        wiki: &str,
        mut sync_once: F,
        should_retry: fn(&E) -> bool,
    ) where
        E: Display,
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<(), E>>,
    {
        let Some(sync) = &self.config else {
            return;
        };
        let mut interval = tokio::time::interval_at(Instant::now() + sync.interval, sync.interval);
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = self.trigger.notified() => {}
            }
            if let Err(error) = self
                .run_with_retry(sync, &mut sync_once, should_retry)
                .await
            {
                tracing::error!(wiki, error = %format!("{error:#}"), "remote sync failed");
            }
        }
    }

    async fn run_with_retry<E, F, Fut>(
        &self,
        sync: &GitSyncConfig,
        sync_once: &mut F,
        should_retry: fn(&E) -> bool,
    ) -> Result<(), E>
    where
        E: Display,
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<(), E>>,
    {
        let _sync = self.lock.lock().await;
        let started = Instant::now();
        let mut attempt = 1;
        let mut delay = sync.retry_initial_delay;
        loop {
            match sync_once().await {
                Ok(()) => return Ok(()),
                Err(error) if should_retry(&error) => {
                    let elapsed = started.elapsed();
                    if elapsed >= sync.retry_max_elapsed {
                        return Err(error);
                    }
                    let sleep_for = delay.min(sync.retry_max_elapsed.saturating_sub(elapsed));
                    tracing::warn!(
                        remote = %sync.remote,
                        attempt,
                        retry_in_secs = sleep_for.as_secs(),
                        error = %error,
                        "remote sync attempt failed; retrying"
                    );
                    tokio::time::sleep(sleep_for).await;
                    attempt += 1;
                    delay = delay.saturating_mul(2).min(sync.retry_max_delay);
                }
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    fn test_config() -> GitSyncConfig {
        GitSyncConfig {
            remote: "origin".into(),
            interval: Duration::from_secs(60),
            retry_max_elapsed: Duration::from_millis(100),
            retry_initial_delay: Duration::from_millis(1),
            retry_max_delay: Duration::from_millis(1),
            ssh_command: None,
        }
    }

    #[tokio::test]
    async fn run_with_retry_retries_until_success() {
        let remote = RemoteSync::new(Some(test_config()));
        let sync = remote.config.as_ref().unwrap();
        let mut attempts = 0;
        let mut sync_once = || {
            attempts += 1;
            let attempt = attempts;
            async move {
                if attempt < 3 {
                    Err("transient")
                } else {
                    Ok(())
                }
            }
        };

        remote
            .run_with_retry(sync, &mut sync_once, |_| true)
            .await
            .unwrap();

        assert_eq!(attempts, 3);
    }

    #[tokio::test]
    async fn run_with_retry_serializes_transactions() {
        let remote = Arc::new(RemoteSync::new(Some(test_config())));
        let sync = remote.config.as_ref().unwrap().clone();
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();

        for _ in 0..2 {
            let remote = remote.clone();
            let sync = sync.clone();
            let active = active.clone();
            let max_active = max_active.clone();
            tasks.push(tokio::spawn(async move {
                let mut sync_once = || {
                    let active = active.clone();
                    let max_active = max_active.clone();
                    async move {
                        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                        max_active.fetch_max(now, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                        Ok::<(), &str>(())
                    }
                };
                remote
                    .run_with_retry(&sync, &mut sync_once, |_| true)
                    .await
                    .unwrap();
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }

        assert_eq!(max_active.load(Ordering::SeqCst), 1);
    }
}
