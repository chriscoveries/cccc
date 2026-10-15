use super::*;

/// A cancelled worker whose join is deferred until any lifecycle guards release.
#[must_use = "dropping this handle waits for the cancelled delivery worker"]
pub(crate) struct DeferredWorkerShutdown {
    worker: Option<DeliveryWorker>,
    key: Key,
}

impl Drop for DeferredWorkerShutdown {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.shutdown();
        }
        clear_in_flight(|item| item.0 == self.key.0 && item.1 == self.key.1);
    }
}

/// Remove and cancel an actor's worker without waiting for its lifecycle guards.
/// Drop the returned handle after releasing those guards to finish shutdown.
pub(crate) fn cancel_actor(group_id: &str, actor_id: &str) -> DeferredWorkerShutdown {
    let key = (group_id.to_owned(), actor_id.to_owned());
    let mut worker = workers()
        .lock()
        .ok()
        .and_then(|mut workers| workers.remove(&key));
    if let Some(worker) = &mut worker {
        worker.cancel();
    }
    DeferredWorkerShutdown { worker, key }
}

pub fn shutdown_actor(group_id: &str, actor_id: &str) {
    drop(cancel_actor(group_id, actor_id));
}

pub fn shutdown_group(group_id: &str) {
    let removed = workers()
        .lock()
        .map(|mut workers| {
            let keys = workers
                .keys()
                .filter(|(worker_group_id, _)| worker_group_id == group_id)
                .cloned()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| workers.remove(&key))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for worker in removed {
        worker.shutdown();
    }
    clear_in_flight(|item| item.0 == group_id);
}

pub fn shutdown_all() {
    let removed = workers()
        .lock()
        .map(|mut workers| {
            std::mem::take(&mut *workers)
                .into_values()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for worker in removed {
        worker.shutdown();
    }
    if let Ok(mut completions) = completions().lock() {
        completions.clear();
    }
    if let Ok(mut pending) = in_flight().lock() {
        pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn cancellation_defers_join_until_the_start_guard_is_released() {
        let key = (
            format!("g_deferred_shutdown_{}", uuid::Uuid::new_v4()),
            "worker".to_owned(),
        );
        let claim = (key.0.clone(), key.1.clone(), "pending-event".to_owned());
        let shutdown;
        let guard = super::super::super::local_headless::hold_start_guard_for_test(&key.0, &key.1)
            .expect("maintenance start guard");
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker_key = key.clone();
        let (waiting_sender, waiting_receiver) = mpsc::channel();
        let (finished_sender, finished_receiver) = mpsc::channel();
        let (job_sender, _job_receiver) = mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || {
            waiting_sender.send(()).expect("announce start guard wait");
            let _start = super::super::super::local_headless::hold_start_guard_for_test(
                &worker_key.0,
                &worker_key.1,
            )
            .expect("worker start guard");
            finished_sender
                .send(worker_cancelled.load(Ordering::Acquire))
                .expect("worker finished");
        });
        workers().lock().expect("workers").insert(
            key.clone(),
            DeliveryWorker {
                sender: Some(job_sender),
                cancelled: Arc::clone(&cancelled),
                thread: Some(thread),
            },
        );
        in_flight().lock().expect("in flight").insert(claim.clone());
        waiting_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("worker attempting start guard");

        let cancel_key = key.clone();
        let (shutdown_sender, shutdown_receiver) = mpsc::channel();
        let cancellation = std::thread::spawn(move || {
            shutdown_sender
                .send(cancel_actor(&cancel_key.0, &cancel_key.1))
                .unwrap_or_else(|_| panic!("shutdown handle receiver"));
        });
        shutdown = shutdown_receiver.recv_timeout(Duration::from_secs(1));
        let returned_while_guard_held = shutdown.is_ok();
        let worker_removed = !workers().lock().expect("workers").contains_key(&key);
        let worker_cancelled = cancelled.load(Ordering::Acquire);
        let claim_retained = in_flight().lock().expect("in flight").contains(&claim);
        let worker_still_waiting =
            matches!(finished_receiver.try_recv(), Err(mpsc::TryRecvError::Empty));

        // Always release the gate before waiting, including when cancellation
        // regresses to joining synchronously, so that failure cannot hang CI.
        drop(guard);
        let shutdown = shutdown.unwrap_or_else(|_| {
            shutdown_receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("shutdown after releasing start guard")
        });
        cancellation.join().expect("cancellation thread");
        drop(shutdown);
        let finished_cancelled = finished_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("worker joined");

        assert!(returned_while_guard_held, "cancellation must not join");
        assert!(worker_removed, "cancellation removes the worker");
        assert!(worker_cancelled, "cancellation signals the worker");
        assert!(claim_retained, "in-flight claims remain until the join");
        assert!(worker_still_waiting, "worker waits on the real start guard");
        assert!(
            finished_cancelled,
            "worker observes cancellation after waiting"
        );
        assert!(!in_flight().lock().expect("in flight").contains(&claim));
    }

    #[test]
    fn actor_and_group_shutdown_remove_workers() {
        let first = ("g_cleanup".to_owned(), "actor-1".to_owned());
        let second = ("g_cleanup".to_owned(), "actor-2".to_owned());
        workers().lock().expect("workers").extend([
            (first.clone(), spawn_worker(&first)),
            (second.clone(), spawn_worker(&second)),
        ]);

        shutdown_actor(&first.0, &first.1);
        assert!(!workers().lock().expect("workers").contains_key(&first));
        shutdown_group(&second.0);
        assert!(!workers().lock().expect("workers").contains_key(&second));
    }
}
