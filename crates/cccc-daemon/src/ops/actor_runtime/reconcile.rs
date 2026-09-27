use cccc_contracts::Event;
use cccc_core::ledger;
use cccc_core::{GroupStore, HomeLayout};
use cccc_runtime::SessionStatus;

use super::runtime_error;
use crate::dispatch::OpError;

pub fn reap_exited() -> Result<Vec<SessionStatus>, OpError> {
    cccc_runtime::reap()
        .map(reconciliable_exits)
        .map_err(runtime_error)
}

fn reconciliable_exits(exited: Vec<SessionStatus>) -> Vec<SessionStatus> {
    exited
        .into_iter()
        .filter(|status| !has_running_replacement(status))
        .collect()
}

fn has_running_replacement(status: &SessionStatus) -> bool {
    cccc_runtime::status(&status.group_id, &status.actor_id).is_ok_and(|current| current.running)
}

pub fn reconcile_exited(home: &HomeLayout, exited: Vec<SessionStatus>) -> Result<(), OpError> {
    let store = GroupStore::new(home.clone()).map_err(OpError::io)?;
    for status in exited {
        reconcile_one(&store, status)?;
    }
    Ok(())
}

fn reconcile_one(store: &GroupStore, status: SessionStatus) -> Result<(), OpError> {
    if has_running_replacement(&status) {
        return Ok(());
    }
    let Ok(group) = store.load(&status.group_id) else {
        return Ok(());
    };
    let Some(actor) = group
        .actors
        .iter()
        .find(|actor| actor.id == status.actor_id)
    else {
        return Ok(());
    };
    if super::super::local_headless::supports(actor) {
        super::super::local_headless::stop(&status.group_id, &status.actor_id)
            .map_err(OpError::io)?;
    }
    // RS-3: an exit inside the admission window is a fast failure. Repeated ones delay the next
    // automatic restart and eventually park the Actor; a session that outlived the window proves
    // its launch worked and clears the epoch.
    let fast_failure = super::super::actor_restart_backoff::is_fast_failure(
        &status.started_at,
        super::super::actor_restart_backoff::now_ms(),
    );
    if fast_failure {
        // The runtime registry keeps the exit code but not provider stderr, so the persisted
        // `last_exit_stderr` slot carries the reason the daemon does have. A provider that
        // refuses to start never reaches this path: its error text is already in the ledger as
        // actor.resume_failed, and counting it here as well would double-count one failure.
        let stderr = match status.exit_code {
            Some(code) => format!("provider exited before admission (exit code {code})"),
            None => "provider exited before admission (no exit code)".to_owned(),
        };
        if let Err(error) = super::super::actor_restart_backoff::record_fast_failure(
            store.home(),
            &status.group_id,
            &status.actor_id,
            status.exit_code.map(|code| code as i32),
            &stderr,
        ) {
            tracing::warn!(
                group_id = %status.group_id,
                actor_id = %status.actor_id,
                %error,
                "failed to record the fast failure for restart backoff"
            );
        }
        // RS-1's exit half: this launch never returned to a launcher, so nothing invalidated the
        // provider binding it was resuming. Without this the next launch would resume the same dead
        // session and only fall back to fresh after failing again.
        if let Err(error) = super::super::runtime_session::invalidate_current_receipt(
            store.home(),
            &status.group_id,
            &status.actor_id,
            &stderr,
        ) {
            tracing::warn!(
                group_id = %status.group_id,
                actor_id = %status.actor_id,
                %error,
                "failed to invalidate the managed session after a fast failure"
            );
        }
    } else if let Err(error) = super::super::actor_restart_backoff::record_first_turn(
        store.home(),
        &status.group_id,
        &status.actor_id,
    ) {
        tracing::warn!(
            group_id = %status.group_id,
            actor_id = %status.actor_id,
            %error,
            "failed to clear the restart backoff epoch for a session that reached admission"
        );
    }
    // Preserve desired lifecycle after a provider exit. A later user-directed
    // message follows the same wake path whether the process exited or was stopped.
    append_exit_event(store, &status.group_id, &status.actor_id, status.exit_code)
}

pub(crate) fn record_process_exit(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    exit_code: Option<u32>,
) -> Result<(), OpError> {
    let store = GroupStore::new(home.clone()).map_err(OpError::io)?;
    let Ok(group) = store.load(group_id) else {
        return Ok(());
    };
    if !group.actors.iter().any(|actor| actor.id == actor_id) {
        return Ok(());
    }
    append_exit_event(&store, group_id, actor_id, exit_code)
}

fn append_exit_event(
    store: &GroupStore,
    group_id: &str,
    actor_id: &str,
    exit_code: Option<u32>,
) -> Result<(), OpError> {
    let mut event = Event::new("actor.stop", group_id);
    event.by = "system".into();
    event.data = serde_json::json!({
        "actor_id": actor_id,
        "reason": "process_exit",
        "exit_code": exit_code,
    })
    .as_object()
    .cloned()
    .unwrap_or_default();
    ledger::append(&store.ledger_path(group_id).map_err(OpError::io)?, &event).map_err(OpError::io)
}

#[cfg(test)]
mod tests {
    use cccc_contracts::{Actor, RunnerKind, RuntimeStateSource};
    use cccc_core::{GroupStore, HomeLayout, ledger};
    use cccc_runtime::{LaunchSpec, SessionStatus};
    use std::collections::BTreeMap;

    use super::reconcile_exited;

    #[test]
    fn app_server_exit_is_recorded_without_disabling_actor() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("test", "").expect("group");
        store
            .mutate(&group.group_id, |doc| {
                let mut actor = Actor::new("peer1");
                actor.runtime_state_source = RuntimeStateSource::ManagedSession;
                doc.actors.push(actor);
                doc.running = true;
                Ok(())
            })
            .expect("add actor");

        let result = reconcile_exited(
            &home,
            vec![SessionStatus {
                group_id: group.group_id.clone(),
                actor_id: "peer1".into(),
                runner: RunnerKind::Pty,
                running: false,
                pid: Some(42),
                started_at: "2026-07-27T00:00:00Z".into(),
                exit_code: Some(7),
            }],
        );
        assert!(result.is_ok());

        let reloaded = store.load(&group.group_id).expect("reload group");
        assert!(reloaded.actors[0].enabled);
        let events = ledger::read_all(&store.ledger_path(&group.group_id).expect("ledger path"))
            .expect("read ledger");
        let event = events.last().expect("exit event");
        assert_eq!(event.kind, "actor.stop");
        assert_eq!(event.data["actor_id"], "peer1");
        assert_eq!(event.data["exit_code"], 7);
    }

    #[test]
    fn terminal_exit_preserves_desired_lifecycle_for_message_auto_wake() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("recoverable terminal", "").expect("group");
        store
            .mutate(&group.group_id, |doc| {
                doc.actors.push(Actor::new("peer1"));
                doc.running = true;
                Ok(())
            })
            .expect("add actor");

        reconcile_exited(
            &home,
            vec![SessionStatus {
                group_id: group.group_id.clone(),
                actor_id: "peer1".into(),
                runner: RunnerKind::Pty,
                running: false,
                pid: Some(42),
                started_at: "2026-08-25T00:00:00Z".into(),
                exit_code: Some(1),
            }],
        )
        .expect("reconcile terminal exit");

        let reloaded = store.load(&group.group_id).expect("reload group");
        assert!(reloaded.running);
        assert!(reloaded.actors[0].enabled);
        let events = ledger::read_all(&store.ledger_path(&group.group_id).expect("ledger path"))
            .expect("read ledger");
        let event = events.last().expect("exit event");
        assert_eq!(event.kind, "actor.stop");
        assert_eq!(event.by, "system");
        assert_eq!(event.data["reason"], "process_exit");
    }

    #[test]
    fn a_fast_exit_is_counted_and_a_healthy_session_clears_the_epoch() {
        use crate::ops::actor_restart_backoff::{self, Gate};
        let temp = tempfile::tempdir().expect("tempdir");
        let home = cccc_core::HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("fast exit", "").expect("group");
        store
            .mutate(&group.group_id, |doc| {
                let mut actor = Actor::new("peer1");
                actor.runtime_state_source = RuntimeStateSource::ManagedSession;
                doc.actors.push(actor);
                doc.running = true;
                Ok(())
            })
            .expect("add actor");

        let just_started =
            chrono::DateTime::from_timestamp_millis(actor_restart_backoff::now_ms() - 1_000)
                .expect("timestamp")
                .to_rfc3339();
        reconcile_exited(
            &home,
            vec![SessionStatus {
                group_id: group.group_id.clone(),
                actor_id: "peer1".into(),
                runner: RunnerKind::Pty,
                running: false,
                pid: Some(43),
                started_at: just_started,
                exit_code: Some(3),
            }],
        )
        .expect("reconcile fast exit");
        assert_eq!(
            actor_restart_backoff::load(&home, &group.group_id, "peer1")
                .expect("load")
                .consecutive_fast_failures,
            1,
            "an exit inside the admission window is one fast failure"
        );
        assert_eq!(
            actor_restart_backoff::gate(&home, &group.group_id, "peer1"),
            Gate::Allow,
            "a single fast failure must not delay the next legitimate wake"
        );

        // A second consecutive fast failure is what the backoff holds back.
        actor_restart_backoff::record_fast_failure(
            &home,
            &group.group_id,
            "peer1",
            Some(3),
            "again",
        )
        .expect("second fast failure");
        assert!(
            matches!(
                actor_restart_backoff::gate(&home, &group.group_id, "peer1"),
                Gate::Wait { .. }
            ),
            "repeated fast failures are held back"
        );

        let long_running =
            chrono::DateTime::from_timestamp_millis(actor_restart_backoff::now_ms() - 600_000)
                .expect("timestamp")
                .to_rfc3339();
        reconcile_exited(
            &home,
            vec![SessionStatus {
                group_id: group.group_id.clone(),
                actor_id: "peer1".into(),
                runner: RunnerKind::Pty,
                running: false,
                pid: Some(44),
                started_at: long_running,
                exit_code: Some(0),
            }],
        )
        .expect("reconcile established exit");
        assert_eq!(
            actor_restart_backoff::gate(&home, &group.group_id, "peer1"),
            Gate::Allow,
            "a session that reached admission clears the backoff epoch"
        );
    }

    #[test]
    fn stale_exit_does_not_disable_a_running_replacement() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("replacement", "").expect("group");
        store
            .mutate(&group.group_id, |doc| {
                doc.actors.push(Actor::new("peer1"));
                doc.running = true;
                Ok(())
            })
            .expect("add actor");
        let current = cccc_runtime::start(LaunchSpec {
            group_id: group.group_id.clone(),
            actor_id: "peer1".into(),
            runner: RunnerKind::Pty,
            command: vec!["sh".into(), "-c".into(), "sleep 30".into()],
            cwd: temp.path().to_path_buf(),
            env: BTreeMap::new(),
            cols: 120,
            rows: 40,
        })
        .expect("replacement runtime");

        reconcile_exited(
            &home,
            vec![SessionStatus {
                group_id: group.group_id.clone(),
                actor_id: "peer1".into(),
                runner: RunnerKind::Pty,
                running: false,
                pid: Some(41),
                started_at: "older-session".into(),
                exit_code: Some(1),
            }],
        )
        .expect("reconcile stale exit");

        let reloaded = store.load(&group.group_id).expect("reload group");
        assert!(reloaded.running);
        assert!(reloaded.actors[0].enabled);
        assert_eq!(
            cccc_runtime::status(&group.group_id, "peer1")
                .expect("current runtime")
                .started_at,
            current.started_at
        );
        cccc_runtime::stop(&group.group_id, "peer1").expect("cleanup");
    }
}
