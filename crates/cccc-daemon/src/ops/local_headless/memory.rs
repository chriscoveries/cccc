//! Exact-session maintenance for idle, directly owned native ACP providers.
use super::super::codex_voice_analyst::AnalystSession;
use super::{Session, run_managed_launch, supervisor};
use cccc_contracts::{Actor, GroupState};
use cccc_core::{GroupDoc, GroupStore, HomeLayout};
use std::io;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemoryCandidate {
    pub generation: String,
    pub provider_pid: u32,
    pub input_sequence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemoryRestartOutcome {
    Skipped,
    Restarted,
}

fn candidate(item: &Session) -> Option<MemoryCandidate> {
    if item.has_terminal() {
        return None;
    }
    item.running().then_some(())?;
    let input_sequence = item
        .input_sequence
        .load(std::sync::atomic::Ordering::Acquire);
    if input_sequence == u64::MAX {
        return None;
    }
    Some(MemoryCandidate {
        generation: item.managed.generation().to_owned(),
        provider_pid: item.managed.memory_provider_pid()?,
        input_sequence,
    })
}

/// PTY-backed managed actor: serialize the provider lifecycle with controlled
/// submissions, then retire only the generation whose terminal input is still
/// unchanged. The runtime gate is released before stop_locked re-enters runtime
/// cleanup; StartGuard keeps a new managed generation out throughout.
pub(crate) fn stop_pty_if_input_unchanged(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    generation: &str,
    checkpoint: &cccc_runtime::InputCheckpoint,
    before_stop: impl FnOnce() -> io::Result<()>,
) -> io::Result<bool> {
    let key = (group_id.to_owned(), actor_id.to_owned());
    let _start = supervisor::StartGuard::acquire(&key)?;
    let Some(item) = supervisor::lookup(&key) else {
        return Ok(false);
    };
    if item.home.root() != home.root()
        || item.managed.generation() != generation
        || !item.has_terminal()
    {
        return Ok(false);
    }
    if cccc_runtime::stop_if_input_unchanged(group_id, actor_id, checkpoint, before_stop)
        .map_err(io::Error::other)?
        .is_none()
    {
        return Ok(false);
    }
    supervisor::stop_locked(&key)?;
    Ok(true)
}

pub(crate) fn memory_candidate(group_id: &str, actor_id: &str) -> Option<MemoryCandidate> {
    let item = supervisor::lookup(&(group_id.to_owned(), actor_id.to_owned()))?;
    candidate(&item)
}

/// The callback checks final owned RSS, the fresh limit and cooldown under the
/// same gate as controlled input. Only a successful protocol idle-close may
/// retire the provider; a skipped check leaves delivery and ownership intact.
#[cfg(all(test, unix))]
pub(crate) fn restart_for_memory(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    expected: &MemoryCandidate,
    admit: impl FnOnce(&GroupDoc, &Actor, &MemoryCandidate) -> io::Result<bool>,
) -> io::Result<MemoryRestartOutcome> {
    restart_for_memory_with_commit(home, group_id, actor_id, expected, admit, || Ok(()))
}

/// `commit` is invoked exactly once after authoritative idle-close succeeds.
/// A busy refusal never reserves cooldown. The exact-resume sidecar remains
/// durable before close; a failed cooldown commit still tears down this closed
/// generation and leaves the sidecar in place for explicit recovery.
pub(crate) fn restart_for_memory_with_commit(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    expected: &MemoryCandidate,
    admit: impl FnOnce(&GroupDoc, &Actor, &MemoryCandidate) -> io::Result<bool>,
    commit: impl FnOnce() -> io::Result<()>,
) -> io::Result<MemoryRestartOutcome> {
    // This binding must precede both gates: joining a cancelled worker can
    // require those gates, so every return releases them before joining it.
    let _delivery_shutdown: super::super::actor_delivery::DeferredWorkerShutdown;
    let Ok(_permit) = crate::runtime_start_gate::permit(home) else {
        return Ok(MemoryRestartOutcome::Skipped);
    };

    // Tests-only MUST3 interposition — ORDERING BOUNDARY: placed before
    // StartGuard::acquire (and before any input_gate take). A managed
    // submit_batch try_acquires this same guard, so an interposed submit can
    // only genuinely ACCEPT here, never defer; once the guard is held below,
    // acceptance is structurally impossible. The future input-record recheck
    // must therefore be evaluated under the guard at the final close/stop,
    // against a record this window can still advance. No-op when unarmed.
    #[cfg(all(test, target_os = "linux"))]
    super::super::actor_memory::must3_gap_tests::interpose_submit(home, group_id, actor_id);

    let key = (group_id.to_owned(), actor_id.to_owned());
    let _start = supervisor::StartGuard::acquire(&key)?;
    let Some(item) = supervisor::lookup(&key) else {
        return Ok(MemoryRestartOutcome::Skipped);
    };
    if item.home.root() != home.root() {
        return Ok(MemoryRestartOutcome::Skipped);
    }
    let Some(current) = candidate(&item).filter(|current| current == expected) else {
        return Ok(MemoryRestartOutcome::Skipped);
    };
    let group = GroupStore::new(home.clone())?.load(group_id)?;
    if !group.running || group.state != GroupState::Active {
        return Ok(MemoryRestartOutcome::Skipped);
    }
    let Some(stored_actor) = group
        .actors
        .iter()
        .find(|actor| actor.id == actor_id && actor.enabled)
    else {
        return Ok(MemoryRestartOutcome::Skipped);
    };
    let mut actor = super::super::actor_profile_runtime::resolve(home, stored_actor)
        .map_err(|error| io::Error::other(error.message))?;
    actor.env = super::super::actor_secrets::effective_values(home, group_id, &actor)
        .map_err(|error| io::Error::other(error.message))?;
    if !AnalystSession::supports_memory_restart(actor.runtime, actor.runtime_mode)
        || actor.runtime != item.managed.runtime()
    {
        return Ok(MemoryRestartOutcome::Skipped);
    }
    let cwd = super::super::actor_runtime::working_directory(&group, &actor)
        .map_err(|error| io::Error::other(error.message))?;
    let config = launch_config(home, &group, &actor, &cwd);
    let required_id = item.managed.thread_id().to_owned();
    if !AnalystSession::native_acp_resume_id(home, &config)
        .is_ok_and(|resume| resume.as_deref() == Some(required_id.as_str()))
    {
        return Ok(MemoryRestartOutcome::Skipped);
    }
    let required = super::super::actor_memory::RequiredResume {
        generation: current.generation.clone(),
        runtime: actor.runtime,
        session_id: required_id.clone(),
    };
    let previous = super::super::actor_memory::required_resume(home, group_id, actor_id)?;
    if previous
        .as_ref()
        .is_some_and(|previous| previous != &required)
    {
        return Ok(MemoryRestartOutcome::Skipped);
    }
    if !admit(&group, &actor, &current)? {
        return Ok(MemoryRestartOutcome::Skipped);
    }
    super::super::actor_memory::require_resume(home, group_id, actor_id, &required)?;
    if !super::block_on_managed(item.managed.close_native_acp_if_idle())? {
        if previous.is_none() {
            super::super::actor_memory::clear_required_resume(
                home,
                group_id,
                actor_id,
                Some(&required),
            )?;
        }
        return Ok(MemoryRestartOutcome::Skipped);
    }
    // A worker may already be waiting in actor.start before its controlled
    // submission. Cancel now, but join only after both admission gates drop.
    _delivery_shutdown = super::super::actor_delivery::cancel_actor(group_id, actor_id);
    let committed = commit();
    // Cleanup must run even if persisting the actual attempt fails. The old
    // protocol has already revoked prompt admission at this point.
    let stopped = supervisor::stop_locked(&key);
    if let Err(primary) = committed {
        return Err(match stopped {
            Ok(()) => super::super::actor_memory::blocked_resume(format!(
                "Could not persist the memory restart cooldown: {primary}"
            )),
            Err(cleanup) => super::super::actor_memory::blocked_resume(format!(
                "Could not persist the memory restart cooldown: {primary}; closed-generation cleanup also failed: {cleanup}"
            )),
        });
    }
    stopped?;
    let launch_home = home.clone();
    let app = run_managed_launch(async move {
        AnalystSession::launch_actor_resume_only(&launch_home, config, required_id).await
    })?;
    supervisor::attach_managed(home, &group, &actor, key, cwd, app)?;
    Ok(MemoryRestartOutcome::Restarted)
}

fn launch_config(
    home: &HomeLayout,
    group: &GroupDoc,
    actor: &Actor,
    cwd: &std::path::Path,
) -> super::super::codex_voice_analyst::ActorLaunchConfig {
    let mut env = actor.env.clone();
    env.insert(
        "CCCC_HOME".into(),
        home.root().to_string_lossy().into_owned(),
    );
    env.insert("CCCC_GROUP_ID".into(), group.group_id.clone());
    env.insert("CCCC_ACTOR_ID".into(), actor.id.clone());
    super::super::codex_mcp::configure_actor_cli(&mut env);
    super::super::codex_voice_analyst::ActorLaunchConfig {
        workdir: cwd.to_path_buf(),
        group_id: group.group_id.clone(),
        actor_id: actor.id.clone(),
        runtime: actor.runtime,
        runtime_mode: actor.runtime_mode,
        command: super::provider_cli::base_command(actor),
        environment: env,
    }
}

#[cfg(all(test, unix))]
#[path = "memory_tests.rs"]
mod tests;

/// Registry observation only. Never expose viewer PID as the provider root.
pub(crate) struct ManagedMemoryObservation {
    pub running: bool,
    pub generation: String,
    pub runtime: cccc_contracts::ActorRuntime,
    pub session_id: String,
    pub roots: Vec<u32>,
    pub environment: std::collections::BTreeMap<String, String>,
    pub command: Vec<String>,
    pub resumed: bool,
}

pub(crate) fn observe_managed(
    home: &HomeLayout,
    group: &str,
    actor: &str,
) -> io::Result<Option<ManagedMemoryObservation>> {
    let Some(item) = supervisor::lookup(&(group.to_owned(), actor.to_owned())) else {
        return Ok(None);
    };
    if item.home.root() != home.root() {
        return Err(io::Error::other(
            "Managed memory observation belongs to a different home",
        ));
    }
    Ok(Some(ManagedMemoryObservation {
        running: item.running(),
        generation: item.managed.generation().to_owned(),
        runtime: item.managed.runtime(),
        session_id: item.managed.thread_id().to_owned(),
        roots: item.managed.owned_process_roots_for_memory_observation(),
        environment: item.managed.tui_environment(),
        command: item.managed.actor_tui_command(),
        resumed: item.managed.resumed_for_memory_observation(),
    }))
}
