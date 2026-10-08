use cccc_contracts::{Actor, GroupState};
use cccc_core::{GroupStore, HomeLayout};

use crate::dispatch::OpError;
use crate::dispatch_concurrency::DispatchLocks;
use crate::ops::{actor_delivery, actor_runtime};
use std::time::Duration;

struct PendingRestore {
    group_id: String,
    actor: Actor,
    session_id: String,
}

fn saved_claude_session(home: &HomeLayout, group: &str, actor: &str) -> Option<String> {
    crate::ops::runtime_session::claude_ownership::saved_session(home, group, actor)
        .ok()
        .flatten()
}

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
        let _ = restore_group(home, &store, &meta.group_id)?;
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
    let mut pending = Vec::new();
    for meta in store.list().map_err(OpError::io)? {
        pending.extend(locks.with_group_write_blocking(&meta.group_id, || {
            restore_group(home, &store, &meta.group_id)
        })?);
    }
    if cccc_core::settings::detach_claude_on_exit(home).map_err(OpError::io)? {
        if let Err(error) = report_unmatched(home, &store) {
            tracing::warn!(message=%error.message, "could not report unmatched Claude jobs");
        }
    }
    let mut failures = 0_u32;
    while !pending.is_empty() && crate::runtime_start_gate::allowed(home) {
        let delay = retry_delay(failures);
        let deadline = std::time::Instant::now() + delay;
        while std::time::Instant::now() < deadline {
            if !crate::runtime_start_gate::allowed(home) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        pending.retain(|item| {
            // Re-read eligibility under the same lock as explicit lifecycle
            // operations; never resurrect an actor stopped during backoff.
            // Provider settlement polling runs outside the dispatch lock.
            let binding = locks.with_group_write_blocking(&item.group_id, || {
                let group = store.load(&item.group_id).map_err(OpError::io)?;
                let Some(actor) = group.actors.iter().find(|a| a.id == item.actor.id && a.created_at == item.actor.created_at && should_restore_actor(group.state, a)) else { return Ok(None); };
                if group.state == GroupState::Stopped || saved_claude_session(home, &item.group_id, &item.actor.id).as_deref() != Some(&item.session_id) { return Ok(None); }
                let owner = actor_runtime::saved_claude_binding(home, &group, actor)?;
                if owner.is_some() {
                    crate::ops::runtime_session::claude_ownership::note_probe(home, &item.group_id, &item.actor.id).map_err(OpError::io)?;
                }
                Ok::<_, OpError>(owner)
            });
            let probe = match binding {
                Ok(Some(owner)) => crate::ops::local_headless::poll_saved_claude_job(&owner.config_dir, &owner.session_id, &owner.workspace)
                    .map(|()| true).map_err(|e| if e.kind() == std::io::ErrorKind::WouldBlock { OpError::new(actor_runtime::RUNTIME_BUSY, e.to_string()) } else { OpError::io(e) }),
                Ok(None) => Ok(false),
                Err(e) => Err(e),
            };
            match probe {
                Ok(false) => return false,
                Err(e) if e.code == actor_runtime::RUNTIME_BUSY => return true,
                Err(e) => { tracing::warn!(message=%e.message, "Claude adoption probe failed; preserving owned job"); return true; }
                Ok(true) => {},
            }
            match locks.with_group_write_blocking(&item.group_id, || {
                retry_pending(home, &store, item)
            }) {
                Ok(keep) => keep,
                Err(error) => {
                    tracing::warn!(group_id=%item.group_id, actor_id=%item.actor.id, message=%error.message, "Claude re-adoption retry ended");
                    false
                }
            }
        });
        failures = failures.saturating_add(1);
    }
    Ok(())
}

fn retry_delay(failures: u32) -> Duration {
    Duration::from_millis(250 * (1_u64 << failures.min(4)))
}

fn retry_pending(
    home: &HomeLayout,
    store: &GroupStore,
    item: &PendingRestore,
) -> Result<bool, OpError> {
    if !crate::runtime_start_gate::allowed(home) {
        return Ok(false);
    }
    let group = store.load(&item.group_id).map_err(OpError::io)?;
    // During restore, stopping one actor can temporarily clear group.running
    // while its siblings are still busy and have no local registration yet.
    // Group Stop has its own durable state; harmless actor metadata edits must
    // not abandon a surviving sibling either.
    let eligible = group.state != GroupState::Stopped
        && group.actors.iter().any(|actor| {
            actor.id == item.actor.id
                && actor.created_at == item.actor.created_at
                && actor.runtime == cccc_contracts::ActorRuntime::Claude
                && should_restore_actor(group.state, actor)
        })
        && saved_claude_session(home, &item.group_id, &item.actor.id).as_deref()
            == Some(&item.session_id);
    if !eligible {
        return Ok(false);
    }
    match actor_runtime::apply(home, &group, &item.actor.id, "actor.restore") {
        Ok(_) => {
            store
                .mutate(&group.group_id, |group| {
                    group.running = true;
                    Ok(())
                })
                .map_err(OpError::io)?;
            actor_delivery::dispatch_unread(home, &group, &item.actor.id);
            Ok(false)
        }
        Err(error) if error.code == actor_runtime::RUNTIME_BUSY => Ok(true),
        Err(error) => Err(error),
    }
}

fn restore_group(
    home: &HomeLayout,
    store: &GroupStore,
    group_id: &str,
) -> Result<Vec<PendingRestore>, OpError> {
    let mut pending = Vec::new();
    let Ok(mut group) = store.load(group_id) else {
        return Ok(pending);
    };
    if cccc_core::group_scope::normalize_actor_scope_keys(&mut group) > 0 {
        group = store
            .mutate(group_id, |current| {
                cccc_core::group_scope::normalize_actor_scope_keys(current);
                Ok(current.clone())
            })
            .map_err(OpError::io)?;
    }
    let detach = cccc_core::settings::detach_claude_on_exit(home).map_err(OpError::io)?;
    if group.state == GroupState::Stopped {
        if detach {
            actor_runtime::stop_group(home, &group)?;
        }
        return Ok(pending);
    }
    for actor in group
        .actors
        .iter()
        .filter(|actor| should_restore_actor(group.state, actor))
    {
        if !group.running {
            if !detach || actor.runtime != cccc_contracts::ActorRuntime::Claude {
                continue;
            }
            // A receipt alone is history, not running intent. Recover durable
            // ownership, or validate a surviving legacy job before adoption.
            let owned = crate::ops::runtime_session::claude_ownership::load(home, group_id, actor)
                .map_err(OpError::io)?
                .is_some();
            if !owned {
                let Some(binding) = actor_runtime::saved_claude_binding(home, &group, actor)?
                else {
                    continue;
                };
                if !crate::ops::local_headless::has_saved_claude_job(
                    &binding.config_dir,
                    &binding.session_id,
                    &binding.workspace,
                )
                .map_err(OpError::io)?
                {
                    continue;
                }
            }
        }
        if deepseek_restore_blocked(home, &group, actor) {
            tracing::info!(
                group_id = %group.group_id,
                actor_id = %actor.id,
                "skipped automatic DeepSeek restore until an explicit actor start"
            );
            continue;
        }
        match actor_runtime::apply(home, &group, &actor.id, "actor.restore") {
            Ok(_) => {
                actor_delivery::dispatch_unread(home, &group, &actor.id);
            }
            Err(error) => {
                if actor.runtime == cccc_contracts::ActorRuntime::Claude
                    && error.code == actor_runtime::RUNTIME_BUSY
                    && cccc_core::settings::detach_claude_on_exit(home).map_err(OpError::io)?
                    && let Some(session_id) = saved_claude_session(home, group_id, &actor.id)
                {
                    pending.push(PendingRestore {
                        group_id: group_id.into(),
                        actor: actor.clone(),
                        session_id,
                    });
                }
                tracing::warn!(
                    group_id = %group.group_id,
                    actor_id = %actor.id,
                    message = %error.message,
                    "failed to restore actor runtime"
                );
            }
        }
    }
    Ok(pending)
}

pub(crate) fn report_unmatched(home: &HomeLayout, store: &GroupStore) -> Result<(), OpError> {
    let mut configurations =
        std::collections::BTreeMap::<std::path::PathBuf, std::collections::HashSet<String>>::new();
    for meta in store.list().map_err(OpError::io)? {
        let group = store.load(&meta.group_id).map_err(OpError::io)?;
        for actor in &group.actors {
            let mut actor = crate::ops::actor_profile_runtime::resolve(home, actor)?;
            actor.env = crate::ops::actor_secrets::effective_values(home, &group.group_id, &actor)?;
            if actor.runtime != cccc_contracts::ActorRuntime::Claude {
                continue;
            }
            let config = crate::ops::codex_voice_analyst::claude_config_dir(&actor.env)
                .map_err(OpError::io)?;
            if let Some(owner) =
                crate::ops::runtime_session::claude_ownership::load(home, &group.group_id, &actor)
                    .map_err(OpError::io)?
            {
                let known = configurations.entry(owner.config_dir).or_default();
                if !owner.session_id.is_empty() {
                    known.insert(owner.session_id);
                }
            }
            let known = configurations.entry(config).or_default();
            if let Some(id) = saved_claude_session(home, &group.group_id, &actor.id) {
                known.insert(id);
            }
        }
    }
    crate::ops::local_headless::report_unmatched_claude_jobs(configurations);
    Ok(())
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
    use super::{deepseek_restore_blocked, should_restore_actor};
    use cccc_contracts::{Actor, ActorRuntime, GroupState};
    use cccc_core::{GroupStore, HomeLayout};

    #[test]
    fn busy_restore_backoff_is_bounded() {
        assert_eq!(super::retry_delay(0), std::time::Duration::from_millis(250));
        assert_eq!(
            super::retry_delay(u32::MAX),
            std::time::Duration::from_secs(4)
        );
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
