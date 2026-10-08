use cccc_core::HomeLayout;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::task::JoinHandle;

pub(crate) struct DiskHealthService {
    cancelled: Arc<AtomicBool>,
    task: JoinHandle<()>,
}
impl DiskHealthService {
    pub(crate) fn start(home: HomeLayout) -> Self {
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let task = tokio::spawn(async move {
            let mut publisher = crate::disk_health::Publisher::default();
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            while !worker_cancelled.load(Ordering::Acquire) {
                interval.tick().await;
                if worker_cancelled.load(Ordering::Acquire) {
                    break;
                }
                let tick_home = home.clone();
                publisher = tokio::task::spawn_blocking(move || {
                    if let Err(error) = publisher.tick(&tick_home) {
                        tracing::warn!(%error, "home-volume disk health publication failed");
                    }
                    publisher
                })
                .await
                .unwrap_or_default();
            }
        });
        Self { cancelled, task }
    }
    pub(crate) async fn finish(self) {
        self.cancelled.store(true, Ordering::Release);
        let mut task = self.task;
        if tokio::time::timeout(Duration::from_millis(500), &mut task)
            .await
            .is_err()
        {
            task.abort();
            let _ = task.await;
        }
    }
}
