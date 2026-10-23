use super::{Session, block_on_managed, managed_runtime, output};
use cccc_contracts::utc_now;
use serde_json::Value;
use std::io;
use std::sync::atomic::Ordering;
use tracing::Instrument;

impl Session {
    pub(super) fn running(&self) -> bool {
        if self.stopped.load(Ordering::Acquire) {
            return false;
        }
        // The native TUI is an attachment, not the managed provider's owner.
        // This must hold even before the exited terminal has been reaped.
        self.managed.process_running()
    }

    pub(super) fn stop(&self) -> io::Result<bool> {
        let _guard = self.stop_lock.lock().map_err(|_| super::poisoned())?;
        if self.stopped.load(Ordering::Acquire) {
            return Ok(false);
        }
        block_on_managed(self.managed.stop(self.managed.generation()).instrument(
            tracing::info_span!("actor_runtime_stop", group_id = %self.group_id, actor_id = %self.actor_id),
        ))?;
        if self.has_terminal.load(Ordering::Acquire) {
            match cccc_runtime::stop(&self.group_id, &self.actor_id) {
                Ok(_) | Err(cccc_runtime::RuntimeError::NotFound(_, _)) => {}
                Err(error) => return Err(io::Error::other(error)),
            }
        }
        self.stopped.store(true, Ordering::Release);
        self.set_status("stopped", None);
        output::emit(self, "headless.session.stopped", serde_json::Map::new());
        Ok(true)
    }

    pub(super) fn stop_after_process_exit(&self) -> bool {
        // A dead observer is not proof that the provider job stopped. Use the
        // same confirmed stop path as actor_stop and retain ownership on error.
        match self.stop() {
            Ok(first) => first,
            Err(error) => {
                self.set_status("error", None);
                tracing::error!(
                    %error,
                    group_id = %self.group_id,
                    actor_id = %self.actor_id,
                    "failed to stop disconnected managed Actor; stop remains retryable"
                );
                false
            }
        }
    }

    /// Record which board task the delivery the runtime was asked to run is about,
    /// under the delivery id it is being asked with. `None` records a delivery that
    /// named no single card: known to be unattributed, which is not the same as
    /// unresolved, so it can never be given anyone else's card later.
    pub(super) fn bind_delivery_task(&self, delivery_id: &str, task: Option<String>) -> bool {
        let Ok(mut tasks) = self.delivery_tasks.lock() else {
            return false;
        };
        if let Some(existing) = tasks.get(delivery_id) {
            return existing == &task;
        }
        // A full table is backpressure, never proof that old deliveries ended.
        // Refuse this admission instead of erasing unobserved turn authority.
        if tasks.len() >= 16 {
            return false;
        }
        tasks.insert(delivery_id.to_owned(), task);
        true
    }

    /// Keep the binding visible until its ActiveTurn is published. Dispatch
    /// treats every unobserved delivery as Unknown, including unattributed ones.
    pub(super) fn delivery_task(&self, delivery_id: &str) -> Option<Option<String>> {
        self.delivery_tasks
            .lock()
            .ok()
            .and_then(|tasks| tasks.get(delivery_id).cloned())
    }

    /// The card of the delivery the runtime named, taken out so it is used once.
    /// The outer option says whether that delivery was registered at all; the inner
    /// one is the card, `None` when the delivery named no single task.
    pub(super) fn take_delivery_task(&self, delivery_id: &str) -> Option<Option<String>> {
        self.delivery_tasks
            .lock()
            .ok()
            .and_then(|mut tasks| tasks.remove(delivery_id))
    }

    /// Drop the card of a delivery that will never start a turn.
    pub(super) fn forget_delivery_task(&self, delivery_id: &str) {
        if let Ok(mut tasks) = self.delivery_tasks.lock() {
            tasks.remove(delivery_id);
        }
    }

    /// Hold the door for one delivery's admission.
    pub(super) fn begin_admission(&self) -> std::sync::MutexGuard<'_, ()> {
        self.admission
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The task the running turn was started for, if any.
    #[cfg(test)]
    pub(super) fn running_turn_task(&self) -> Option<String> {
        self.active_turn
            .lock()
            .ok()
            .and_then(|turn| turn.as_ref().and_then(|turn| turn.task.clone()))
    }

    pub(super) fn set_status(&self, status: &str, task_id: Option<String>) {
        self.set_status_with_reason(status, task_id, None);
    }

    /// Set the session status and, for a non-nominal status, the provider's
    /// reported cause. A later status change clears any stale reason so a
    /// recovered turn does not keep displaying the previous failure.
    pub(super) fn set_status_with_reason(
        &self,
        status: &str,
        task_id: Option<String>,
        reason: Option<String>,
    ) {
        if let Ok(mut state) = self.status.lock() {
            state.status = status.to_owned();
            state.task_id = task_id;
            state.updated_at = utc_now();
            state.reason = reason;
            if status == "stopped" {
                state.pid = None;
            }
        }
    }

    pub(super) fn attach_terminal(&self, pid: Option<u32>) {
        self.has_terminal.store(true, Ordering::Release);
        if let Ok(mut state) = self.status.lock() {
            state.pid = pid;
            state.updated_at = utc_now();
        }
    }

    pub(super) fn detach_viewer(&self) -> io::Result<bool> {
        let _guard = self.stop_lock.lock().map_err(|_| super::poisoned())?;
        if self.stopped.load(Ordering::Acquire)
            || cccc_runtime::status(&self.group_id, &self.actor_id)
                .is_ok_and(|status| status.running)
        {
            return Ok(false);
        }
        let detached = self.has_terminal.swap(false, Ordering::AcqRel);
        if detached && let Ok(mut status) = self.status.lock() {
            status.pid = None;
            status.updated_at = utc_now();
        }
        Ok(detached)
    }

    pub(super) fn reattach_viewer(&self) -> io::Result<()> {
        if self.managed.structured_only() {
            return Ok(());
        }
        let _guard = self.stop_lock.lock().map_err(|_| super::poisoned())?;
        if !self.running() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "managed provider is not running",
            ));
        }
        if cccc_runtime::status(&self.group_id, &self.actor_id).is_ok_and(|status| status.running) {
            return Ok(());
        }
        let viewer = self
            .viewer
            .lock()
            .map_err(|_| super::poisoned())?
            .clone()
            .ok_or_else(|| io::Error::other("managed viewer launch configuration is missing"))?;
        let history = super::super::actor_runtime::terminal_history::config(
            &self.home,
            &self.group_id,
            &self.actor_id,
        )?;
        let status = cccc_runtime::start_with_history(
            cccc_runtime::LaunchSpec {
                group_id: self.group_id.clone(),
                actor_id: self.actor_id.clone(),
                runner: cccc_contracts::RunnerKind::Pty,
                command: viewer.command,
                cwd: viewer.cwd,
                env: viewer.env,
                cols: 120,
                rows: 40,
            },
            history,
        )
        .map_err(io::Error::other)?;
        self.attach_terminal(status.pid);
        output::emit(
            self,
            "headless.session.viewer_attached",
            serde_json::Map::new(),
        );
        Ok(())
    }

    pub(super) fn has_terminal(&self) -> bool {
        self.has_terminal.load(Ordering::Acquire)
    }

    pub(super) fn respond_error(&self, id: Value, error: Value) -> io::Result<()> {
        managed_runtime().block_on(self.managed.respond_error(id, error))
    }
}
