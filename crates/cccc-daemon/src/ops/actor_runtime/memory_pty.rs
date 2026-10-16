//! Explicit PTY policy: fresh process/session after observed ten-minute quiet.
//! No provider idle proof, adopted PID signalling or native-resume fallback.
use super::{actor_profile_runtime, delivery, stop_registered};
use crate::ops::{actor_delivery, actor_memory, runtime_session};
use cccc_core::{GroupDoc, GroupStore, HomeLayout};
use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FreshOutcome {
    Skipped,
    Restarted,
}

pub(crate) fn restart_for_memory_pty(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    expected: &actor_memory::observation::Observation,
    admit: impl FnOnce(&GroupDoc, &cccc_contracts::Actor) -> io::Result<bool>,
    commit: impl FnOnce() -> io::Result<()>,
) -> io::Result<FreshOutcome> {
    // A cancelled worker may need this permit. Drop it before deferred join.
    let _shutdown: actor_delivery::DeferredWorkerShutdown;
    let Ok(_permit) = crate::runtime_start_gate::permit(home) else {
        return Ok(FreshOutcome::Skipped);
    };
    let fresh = GroupStore::new(home.clone())?.load(group_id)?;
    if !fresh.running || fresh.state != cccc_contracts::GroupState::Active {
        return Ok(FreshOutcome::Skipped);
    }
    let Some(stored) = fresh.actors.iter().find(|a| a.id == actor_id && a.enabled) else {
        return Ok(FreshOutcome::Skipped);
    };
    let actor =
        actor_profile_runtime::resolve(home, stored).map_err(|e| io::Error::other(e.message))?;
    if actor_memory::pty::is_native_acp(&actor)
        || !actor.memory_limit_restart_pty
        || !actor_memory::pty::quiet_live(group_id, actor_id)
    {
        return Ok(FreshOutcome::Skipped);
    }
    // Opting in to PTY maintenance is never explicit New Session authority.
    if actor_memory::required_resume(home, group_id, actor_id)?.is_some() {
        return Ok(FreshOutcome::Skipped);
    }
    if matches!(&expected.identity, actor_memory::observation::Identity::Managed { runtime, .. } if *runtime != actor.runtime)
    {
        return Ok(FreshOutcome::Skipped);
    }
    if !crate::ops::local_headless::supports(&actor) && super::is_structured(&actor) {
        return Ok(FreshOutcome::Skipped);
    }
    let current = actor_memory::observation::observe(home, &fresh, &actor)
        .ok()
        .flatten();
    if current.as_ref().map(|sample| &sample.identity) != Some(&expected.identity)
        || !admit(&fresh, &actor)?
    {
        return Ok(FreshOutcome::Skipped);
    }
    if actor_memory::required_resume(home, group_id, actor_id)?.is_some()
        || !actor_memory::pty::quiet_live(group_id, actor_id)
    {
        return Ok(FreshOutcome::Skipped);
    }

    // Tests-only MUST3 interposition — ORDERING BOUNDARY: this point holds no
    // gate the submit needs (no input_gate), and the future recheck must take
    // its guard only AFTER here. A real, armed submit may land between the
    // final quiet recheck above and the commit/stop below, so the submit-in-gap
    // regression is a genuine interleaving, never a written activity field.
    // No-op in every non-armed (and every non-test) run.
    #[cfg(all(test, target_os = "linux"))]
    actor_memory::must3_gap_tests::interpose_submit(home, group_id, actor_id);

    // Final admitted attempt boundary: persistence failure must leave the
    // worker, owned provider and receipt untouched. A committed attempt is
    // followed directly by the ordinary owned stop (never a sampled-PID signal).
    commit()?;
    _shutdown = actor_delivery::cancel_actor(group_id, actor_id);
    stop_registered(&fresh, actor_id).map_err(|e| io::Error::other(e.message))?;
    // A concurrent durable native obligation must survive every failure path.
    if actor_memory::required_resume(home, group_id, actor_id)?.is_some() {
        return Err(actor_memory::blocked_resume(
            "Native exact-resume obligation appeared during PTY maintenance",
        ));
    }
    // actor.restart would resume the old saved ID. Remove only the selected
    // provider receipt after confirmed stop; never clear actor_memory_required.
    runtime_session::remove(home, group_id, actor_id)?;
    if crate::ops::local_headless::supports(&actor) {
        delivery::start_managed(home, &fresh, &actor).map_err(|e| io::Error::other(e.message))?;
        if !crate::ops::local_headless::memory::observe_managed(home, group_id, actor_id)?
            .is_some_and(|owner| {
                let distinct = match &expected.identity {
                    actor_memory::observation::Identity::Managed { session_id, .. } => {
                        owner.session_id != *session_id
                    }
                    _ => true,
                };
                owner.running && !owner.resumed && distinct
            })
        {
            let _ = stop_registered(&fresh, actor_id);
            return Err(io::Error::other(
                "PTY memory maintenance did not create a fresh session",
            ));
        }
    } else if !super::is_structured(&actor) {
        delivery::start_terminal(home, &fresh, &actor).map_err(|e| io::Error::other(e.message))?;
        if !cccc_runtime::status(group_id, actor_id).is_ok_and(|status| status.running) {
            return Err(io::Error::other("Fresh PTY replacement is not running"));
        }
    } else {
        return Err(io::Error::other(
            "No fresh PTY launch adapter for selected runtime",
        ));
    }
    Ok(FreshOutcome::Restarted)
}
