//! Automatic startup shares maintenance admission without entering manual stop policy.
use super::{
    actor_is_running, actor_profile_runtime, enforce_required_memory_resume, environment,
    is_structured, launch_error, runtime_error, stop_registered, terminal_history,
    working_directory,
};
use crate::dispatch::OpError;
use crate::ops::{capabilities, deepseek_runtime, local_headless, runtime_mcp};
use cccc_contracts::{Actor, ActorRuntime, RunnerKind};
use cccc_core::{GroupDoc, HomeLayout};
use cccc_runtime::LaunchSpec;
use std::sync::atomic::{AtomicBool, Ordering};

/// A cancelled worker may have waited behind an exact-session memory restart.
/// Admit before checking or retiring ownership, then leave the replacement alone.
pub(crate) fn start_for_delivery(
    home: &HomeLayout,
    group: &GroupDoc,
    actor_id: &str,
    cancelled: &AtomicBool,
) -> Result<bool, OpError> {
    if cancelled.load(Ordering::Acquire) {
        return Ok(false);
    }
    let stored_actor = group
        .actors
        .iter()
        .find(|actor| actor.id == actor_id)
        .ok_or_else(|| OpError::new("not_found", format!("actor not found: {actor_id}")))?;
    #[cfg(all(test, unix))]
    super::memory_delivery_tests::before_start_permit(home, &group.group_id, actor_id);
    let _permit = match crate::runtime_start_gate::permit(home) {
        Ok(permit) => permit,
        Err(_) if cancelled.load(Ordering::Acquire) => return Ok(false),
        Err(message) => return Err(OpError::new("runtime_shutting_down", message)),
    };
    if cancelled.load(Ordering::Acquire) {
        return Ok(false);
    }
    let actor = actor_profile_runtime::resolve(home, stored_actor)?;
    if actor_id == "voice-secretary" || actor.internal_kind.as_deref() == Some("voice_secretary") {
        return Err(OpError::new(
            "global_secretary_owner",
            "This Group's secretary is managed by the global Voice Secretary; it is not an Actor runtime",
        ));
    }
    if actor_is_running(group, &actor) {
        local_headless::ensure_viewer(&group.group_id, actor_id).map_err(OpError::io)?;
        capabilities::apply_actor_startup_baseline(home, group, &actor);
        return Ok(true);
    }
    enforce_required_memory_resume(home, group, &actor)?;
    // A disconnected managed registration may now be configured for a different
    // backend. Preserve automatic backend transitions under the same admission.
    stop_registered(group, actor_id)?;
    capabilities::apply_actor_startup_baseline(home, group, &actor);
    if actor.runtime == ActorRuntime::Deepseek {
        deepseek_runtime::apply(home, group, &actor, "actor.start")?;
    } else if local_headless::supports(&actor) {
        start_managed(home, group, &actor)?;
    } else if !is_structured(&actor) {
        start_terminal(home, group, &actor)?;
    }
    Ok(true)
}

// Admission is held by start_for_delivery. These launch sequences deliberately
// do not call the manual lifecycle helpers, which acquire their own permit and
// may apply an explicit resume/restart policy.
fn start_managed(home: &HomeLayout, group: &GroupDoc, actor: &Actor) -> Result<(), OpError> {
    let mut actor = environment::resolve_launch_actor(home, group, actor)?;
    let cwd = working_directory(group, &actor)?;
    let mut env = environment::launch_env(home, group, &actor);
    if local_headless::uses_managed_provider_cli(&actor) {
        runtime_mcp::prepare(home, actor.runtime, &actor.command, &cwd, &mut env)?;
    }
    actor.env = env;
    local_headless::start(home, group, &actor).map_err(launch_error)
}

fn start_terminal(home: &HomeLayout, group: &GroupDoc, actor: &Actor) -> Result<(), OpError> {
    let actor = environment::resolve_launch_actor(home, group, actor)?;
    let command = if actor.command.is_empty() {
        cccc_runtime::default_command(actor.runtime)
    } else {
        actor.command.clone()
    };
    let cwd = working_directory(group, &actor)?;
    let mut env = environment::launch_env(home, group, &actor);
    runtime_mcp::prepare(home, actor.runtime, &command, &cwd, &mut env)?;
    let command = cccc_runtime::resolve_command_executable(&command, &env);
    let history =
        terminal_history::config(home, &group.group_id, &actor.id).map_err(OpError::io)?;
    cccc_runtime::start_with_history(
        LaunchSpec {
            group_id: group.group_id.clone(),
            actor_id: actor.id.clone(),
            runner: RunnerKind::Pty,
            command,
            cwd,
            env,
            cols: 120,
            rows: 40,
        },
        history,
    )
    .map_err(runtime_error)?;
    Ok(())
}
