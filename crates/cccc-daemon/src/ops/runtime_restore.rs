use cccc_contracts::{Actor, Event, GroupState};
use cccc_core::ledger;
use cccc_core::{GroupDoc, GroupStore, HomeLayout};
use cccc_runtime::SessionStatus;
use serde_json::json;

use crate::dispatch::OpError;
use crate::dispatch_concurrency::DispatchLocks;
use crate::ops::{actor_delivery, actor_runtime};

pub fn spawn(home: HomeLayout, locks: DispatchLocks) {
    let result = std::thread::Builder::new()
        .name("cccc-runtime-restore".into())
        .spawn(move || {
            if let Err(error) = restore_running_serialized(&home, &locks) {
                tracing::warn!(message = %error.message, "failed to restore running runtimes");
            }
        });
    if let Err(error) = result {
        tracing::warn!(%error, "failed to spawn runtime restore worker");
    }
}

#[cfg(test)]
pub fn restore_running(home: &HomeLayout) -> Result<(), OpError> {
    settle_stranded(home)?;
    let store = GroupStore::new(home.clone()).map_err(OpError::io)?;
    for meta in store.list().map_err(OpError::io)? {
        restore_group(home, &store, &meta.group_id)?;
    }
    Ok(())
}

pub(crate) fn settle_stranded(home: &HomeLayout) -> Result<(), OpError> {
    let store = GroupStore::new(home.clone()).map_err(OpError::io)?;
    for meta in store.list().map_err(OpError::io)? {
        let Ok(group) = store.load(&meta.group_id) else {
            continue;
        };
        let settled = crate::ops::runtime_delivery::settle_stranded_claims(home, &group)?;
        if settled > 0 {
            tracing::warn!(
                group_id = %meta.group_id,
                settled,
                "settled stranded runtime delivery claims before daemon IPC startup"
            );
        }
    }
    Ok(())
}

fn restore_running_serialized(home: &HomeLayout, locks: &DispatchLocks) -> Result<(), OpError> {
    let store = GroupStore::new(home.clone()).map_err(OpError::io)?;
    for meta in store.list().map_err(OpError::io)? {
        locks.with_group_write_blocking(&meta.group_id, || {
            restore_group(home, &store, &meta.group_id)
        })?;
    }
    Ok(())
}

fn restore_group(home: &HomeLayout, store: &GroupStore, group_id: &str) -> Result<(), OpError> {
    let Ok(mut group) = store.load(group_id) else {
        return Ok(());
    };
    if cccc_core::group_scope::normalize_actor_scope_keys(&mut group) > 0 {
        group = store
            .mutate(group_id, |current| {
                cccc_core::group_scope::normalize_actor_scope_keys(current);
                Ok(current.clone())
            })
            .map_err(OpError::io)?;
    }
    if !group.running || group.state == cccc_contracts::GroupState::Stopped {
        return Ok(());
    }
    for actor in group
        .actors
        .iter()
        .filter(|actor| should_restore_actor(group.state, actor))
    {
        if deepseek_restore_blocked(home, &group, actor) {
            tracing::info!(
                group_id = %group.group_id,
                actor_id = %actor.id,
                "skipped automatic DeepSeek restore until an explicit actor start"
            );
            continue;
        }
        match actor_runtime::apply(home, &group, &actor.id, "actor.restore") {
            Ok(status) => {
                record_respawn(home, &group, actor, status.as_ref());
                actor_delivery::dispatch_unread(home, &group, &actor.id);
            }
            Err(error) => {
                tracing::warn!(
                    group_id = %group.group_id,
                    actor_id = %actor.id,
                    message = %error.message,
                    "failed to restore actor runtime"
                );
            }
        }
    }
    Ok(())
}

/// A restore is a real lifecycle transition, so it belongs in the ledger.
///
/// The daemon respawns every enabled actor runtime on service start, but that
/// launch path appends no lifecycle event. The newest lifecycle event for a
/// respawned actor therefore stays at whatever preceded it — usually a stop —
/// while the daemon is meanwhile reporting the actor as running with a live pid.
/// Anything that reasons from the ledger (card drives, watchdogs, "is this lane
/// live?" checks, post-mortems) then reads a running actor as stopped, and the
/// divergence silently widens with every restart.
///
/// Fail-soft on purpose: a ledger append that cannot be written must never stop
/// an actor from coming up. A missing event is recoverable by the reconciler; a
/// lane that does not launch is not.
fn record_respawn(
    home: &HomeLayout,
    group: &GroupDoc,
    actor: &Actor,
    status: Option<&SessionStatus>,
) {
    let path = match GroupStore::new(home.clone()).and_then(|store| store.ledger_path(&group.group_id))
    {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!(
                group_id = %group.group_id,
                actor_id = %actor.id,
                %error,
                "failed to resolve ledger path for actor respawn"
            );
            return;
        }
    };
    let mut event = Event::new("actor.respawn", &group.group_id);
    event.by = "system".into();
    event.data = json!({
        "actor_id": actor.id,
        "runtime": serde_json::to_value(actor.runtime).unwrap_or(json!("unknown")),
        "runner": serde_json::to_value(actor.runtime.runner()).unwrap_or(json!("unknown")),
        "pid": status.and_then(|status| status.pid),
        "started_at": status.map(|status| status.started_at.clone()),
    })
    .as_object()
    .cloned()
    .unwrap_or_default();
    if let Err(error) = ledger::append(&path, &event) {
        tracing::warn!(
            group_id = %group.group_id,
            actor_id = %actor.id,
            %error,
            "failed to record actor respawn; ledger may report a live actor as stopped"
        );
    }
}

fn should_restore_actor(state: GroupState, actor: &Actor) -> bool {
    actor.enabled
        && !(state == GroupState::Paused && actor.runner == cccc_contracts::RunnerKind::Headless)
}

fn deepseek_restore_blocked(home: &HomeLayout, group: &cccc_core::GroupDoc, actor: &Actor) -> bool {
    actor.runtime == cccc_contracts::ActorRuntime::Deepseek
        && crate::ops::deepseek_runtime::manual_restart_required(home, group, actor)
}

#[cfg(test)]
mod tests {
    use super::{deepseek_restore_blocked, record_respawn, should_restore_actor};
    use cccc_contracts::{Actor, ActorRuntime, GroupState, RunnerKind};
    use cccc_core::ledger;
    use cccc_core::{GroupStore, HomeLayout};
    use cccc_runtime::SessionStatus;
    use serde_json::json;

    #[test]
    fn respawn_is_recorded_in_the_ledger_with_the_actors_identity() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("respawn ledger", "").expect("group");

        let mut actor = Actor::new("peer1");
        actor.runtime = ActorRuntime::Opencode;
        let status = SessionStatus {
            group_id: group.group_id.clone(),
            actor_id: actor.id.clone(),
            runner: RunnerKind::Pty,
            running: true,
            pid: Some(4242),
            started_at: "2026-09-27T21:30:00Z".into(),
            exit_code: None,
        };

        record_respawn(&home, &group, &actor, Some(&status));

        let events = ledger::read_all(&store.ledger_path(&group.group_id).expect("path"))
            .expect("read ledger");
        let respawn = events
            .iter()
            .find(|event| event.kind == "actor.respawn")
            .expect("actor.respawn recorded");
        assert_eq!(respawn.by, "system");
        assert_eq!(respawn.data["actor_id"], json!("peer1"));
        assert_eq!(respawn.data["runtime"], json!("opencode"));
        assert_eq!(respawn.data["pid"], json!(4242));
        assert_eq!(respawn.data["started_at"], json!("2026-09-27T21:30:00Z"));
    }

    /// A headless restore has no session status, so it records the transition
    /// with a null pid rather than skipping the event. Losing the event is the
    /// exact drift this change exists to close.
    #[test]
    fn respawn_without_a_session_status_still_records_the_transition() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("respawn headless", "").expect("group");

        let mut actor = Actor::new("headless1");
        actor.runtime = ActorRuntime::Deepseek;

        record_respawn(&home, &group, &actor, None);

        let events = ledger::read_all(&store.ledger_path(&group.group_id).expect("path"))
            .expect("read ledger");
        let respawn = events
            .iter()
            .find(|event| event.kind == "actor.respawn")
            .expect("actor.respawn recorded even without a session status");
        assert_eq!(respawn.data["actor_id"], json!("headless1"));
        assert!(respawn.data["pid"].is_null());
        assert!(respawn.data["started_at"].is_null());
    }

    #[test]
    fn paused_groups_restore_terminal_runtimes_but_not_non_terminal_runtimes() {
        let mut actor = Actor::new("peer1");
        actor.runtime = ActorRuntime::Deepseek;
        actor.normalize_runtime_constraints();
        assert!(!should_restore_actor(GroupState::Paused, &actor));
        assert!(should_restore_actor(GroupState::Active, &actor));

        actor.runtime = ActorRuntime::Claude;
        actor.normalize_runtime_constraints();
        assert!(should_restore_actor(GroupState::Paused, &actor));
    }

    #[test]
    fn durable_deepseek_gate_blocks_automatic_restore_until_a_new_generation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let mut group = store.create("deepseek restore", "").expect("group");
        let mut actor = Actor::new("deepseek");
        actor.runtime = ActorRuntime::Deepseek;
        group.actors.push(actor.clone());
        store.save(&group).expect("save");

        cccc_core::deepseek_restart_gate::record_running_generation(
            &home,
            &group.group_id,
            &actor.id,
            &actor.created_at,
            "launch-1",
        )
        .expect("record generation");
        cccc_core::deepseek_restart_gate::require_manual_restart(
            &home,
            &group.group_id,
            &actor.id,
            &actor.created_at,
            "launch-1",
            "credential_unavailable",
        )
        .expect("close gate");
        assert!(deepseek_restore_blocked(&home, &group, &actor));

        cccc_core::deepseek_restart_gate::record_running_generation(
            &home,
            &group.group_id,
            &actor.id,
            &actor.created_at,
            "launch-2",
        )
        .expect("record explicit restart generation");
        assert!(!deepseek_restore_blocked(&home, &group, &actor));
    }
}
