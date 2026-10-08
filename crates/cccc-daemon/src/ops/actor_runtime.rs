use cccc_contracts::{Actor, ActorRuntime, RunnerKind};
use cccc_core::{GroupDoc, GroupStore, HomeLayout};
use cccc_runtime::{LaunchSpec, SessionStatus};
use std::path::PathBuf;

use crate::dispatch::OpError;
use crate::ops::actor_profile_runtime;

mod environment;
mod persistence;
mod reconcile;
pub(crate) mod terminal_history;
#[cfg(test)]
mod untrusted_workspace_tests;
pub use persistence::persist_lifecycle;
pub(crate) use reconcile::record_process_exit;
pub use reconcile::{reap_exited, reconcile_exited};

/// Claude Code refused the Actor's workspace; only the operator can accept its trust prompt.
pub(crate) const CLAUDE_WORKSPACE_UNTRUSTED: &str = "claude_workspace_untrusted";
pub(crate) const CLAUDE_RESUME_FAILED: &str = "claude_resume_failed";
pub(crate) const RUNTIME_BUSY: &str = "runtime_busy";

pub fn apply(
    home: &HomeLayout,
    group: &GroupDoc,
    actor_id: &str,
    kind: &str,
) -> Result<Option<SessionStatus>, OpError> {
    let stored_actor = group
        .actors
        .iter()
        .find(|actor| actor.id == actor_id)
        .ok_or_else(|| OpError::new("not_found", format!("actor not found: {actor_id}")))?;
    // Configuration may already name a different backend. Lifecycle ownership
    // comes from the registries, not from the next launch configuration.
    if stored_actor.runtime.is_web_model()
        && matches!(kind, "actor.stop" | "actor.restart" | "actor.new_session")
    {
        cccc_core::web_model_connectors::interrupt_automatic_pairings(
            home,
            &group.group_id,
            Some(actor_id),
        )
        .map_err(OpError::io)?;
    }
    if kind == "actor.stop" {
        return stop_registered(home, group, actor_id, true);
    }
    let actor = actor_profile_runtime::resolve(home, stored_actor)?;
    if !matches!(kind, "actor.restart" | "actor.new_session")
        && actor_is_running(group, stored_actor)
    {
        // Saving config does not restart an existing session. Explicit restart
        // applies it; start remains idempotent across backend changes too.
        super::local_headless::ensure_viewer(&group.group_id, actor_id).map_err(OpError::io)?;
        super::capabilities::apply_actor_startup_baseline(home, group, &actor);
        return Ok(
            if super::local_headless::running(&group.group_id, actor_id)
                || super::deepseek_runtime::running(&group.group_id, actor_id)
            {
                None
            } else {
                status(&group.group_id, actor_id).filter(|status| status.running)
            },
        );
    }
    // Also retire a disconnected registration whose previous cleanup failed.
    stop_registered(
        home,
        group,
        actor_id,
        matches!(kind, "actor.restart" | "actor.new_session"),
    )?;
    if kind == "actor.new_session" {
        // Pending trust recovery holds the start guard until its launch is
        // attached or discarded. Retire its ownership before removing the
        // receipt, so a late old launch cannot undo the explicit reset.
        super::runtime_session::remove(home, &group.group_id, actor_id).map_err(OpError::io)?;
    }
    super::capabilities::apply_actor_startup_baseline(home, group, &actor);
    if actor.runtime == ActorRuntime::Deepseek {
        super::deepseek_runtime::apply(home, group, &actor, "actor.start")?;
        return Ok(None);
    }
    if super::local_headless::supports(&actor) {
        start_local_headless(home, group, &actor)?;
        return Ok(None);
    }
    if is_structured(&actor) {
        return Ok(None);
    }
    start(home, group, &actor).map(Some)
}

fn start_local_headless(home: &HomeLayout, group: &GroupDoc, actor: &Actor) -> Result<(), OpError> {
    let mut actor = environment::resolve_launch_actor(home, group, actor)?;
    let cwd = working_directory(group, &actor)?;
    let mut env = environment::launch_env(home, group, &actor);
    if super::local_headless::uses_managed_provider_cli(&actor) {
        super::runtime_mcp::prepare(home, actor.runtime, &actor.command, &cwd, &mut env)?;
    }
    actor.env = env;
    let _start_permit = crate::runtime_start_gate::permit(home)
        .map_err(|message| OpError::new("runtime_shutting_down", message))?;
    super::local_headless::start(home, group, &actor).map_err(|error| {
        if actor.runtime == ActorRuntime::Claude
            && cccc_core::settings::detach_claude_on_exit(home).unwrap_or(false)
            && error.kind() == std::io::ErrorKind::WouldBlock
        {
            OpError::new(RUNTIME_BUSY, error.to_string())
        } else {
            launch_error(error)
        }
    })
}

fn launch_error(error: std::io::Error) -> OpError {
    if super::runtime_session::is_claude_resume_blocked(&error) {
        return OpError::new(CLAUDE_RESUME_FAILED, error.to_string());
    }
    let Some(workspace) = super::codex_voice_analyst::untrusted_claude_workspace(&error) else {
        return OpError::io(error);
    };
    let mut op_error = OpError::new(CLAUDE_WORKSPACE_UNTRUSTED, error.to_string());
    op_error
        .details
        .insert("workspace".into(), serde_json::json!(workspace));
    op_error
}

/// A rollback restart refused for the same untrusted workspace is not a separate failure: the
/// original error already names the workspace to trust, and the Actor simply stays stopped.
pub(super) fn same_untrusted_workspace(original: &OpError, restart: &OpError) -> bool {
    original.code == CLAUDE_WORKSPACE_UNTRUSTED
        && restart.code == original.code
        && restart.details.get("workspace") == original.details.get("workspace")
}

fn start(home: &HomeLayout, group: &GroupDoc, actor: &Actor) -> Result<SessionStatus, OpError> {
    let actor = environment::resolve_launch_actor(home, group, actor)?;
    let command = if actor.command.is_empty() {
        cccc_runtime::default_command(actor.runtime)
    } else {
        actor.command.clone()
    };
    let cwd = working_directory(group, &actor)?;
    let mut env = environment::launch_env(home, group, &actor);
    super::runtime_mcp::prepare(home, actor.runtime, &command, &cwd, &mut env)?;
    let _start_permit = crate::runtime_start_gate::permit(home)
        .map_err(|message| OpError::new("runtime_shutting_down", message))?;
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
    .map_err(runtime_error)
}

pub(super) fn stop(group: &GroupDoc, actor_id: &str) -> Result<Option<SessionStatus>, OpError> {
    match cccc_runtime::stop(&group.group_id, actor_id) {
        Ok(status) => Ok(Some(status)),
        Err(cccc_runtime::RuntimeError::NotFound(_, _)) => Ok(None),
        Err(error) => Err(runtime_error(error)),
    }
}

fn stop_registered(
    home: &HomeLayout,
    group: &GroupDoc,
    actor_id: &str,
    stop_saved: bool,
) -> Result<Option<SessionStatus>, OpError> {
    let result = (|| {
        if stop_saved
            && cccc_core::settings::detach_claude_on_exit(home).map_err(OpError::io)?
            && super::local_headless::registered_running(&group.group_id, actor_id).is_none()
            && let Some(stored) = group.actors.iter().find(|actor| actor.id == actor_id)
            && let Some(owner) = saved_claude_binding(home, group, stored)?
        {
            if owner.session_id.is_empty() {
                return Err(OpError::new(
                    CLAUDE_RESUME_FAILED,
                    super::runtime_session::claude_ownership::guidance(&group.group_id, actor_id),
                ));
            }
            super::local_headless::stop_saved_claude_job(
                &owner.config_dir,
                &owner.session_id,
                &owner.workspace,
            )
            .map_err(OpError::io)?;
        }
        super::local_headless::stop(&group.group_id, actor_id).map_err(OpError::io)?;
        super::deepseek_runtime::stop(&group.group_id, actor_id);
        let status = stop(group, actor_id)?;
        if stop_saved && cccc_core::settings::detach_claude_on_exit(home).map_err(OpError::io)? {
            super::runtime_session::claude_ownership::remove(home, &group.group_id, actor_id)
                .map_err(OpError::io)?;
        }
        Ok(status)
    })();
    result.map_err(|mut error: OpError| {
        error
            .details
            .insert("lifecycle_stage".into(), serde_json::json!("stop"));
        error
    })
}

pub fn status(group_id: &str, actor_id: &str) -> Option<SessionStatus> {
    cccc_runtime::status(group_id, actor_id).ok()
}

#[must_use]
pub fn is_structured(actor: &Actor) -> bool {
    !super::local_headless::uses_managed_session(actor)
        && (actor.runner == RunnerKind::Headless || actor.runtime.is_web_model())
}

pub fn start_group(home: &HomeLayout, group: &GroupDoc) -> Result<Vec<SessionStatus>, OpError> {
    let mut statuses = Vec::new();
    let mut started_actor_ids = Vec::new();
    for actor in group.actors.iter().filter(|actor| actor.enabled) {
        let was_running = actor_is_running(group, actor);
        match apply(home, group, &actor.id, "actor.start") {
            Ok(status) => {
                if !was_running && actor_is_running(group, actor) {
                    started_actor_ids.push(actor.id.clone());
                }
                if let Some(status) = status {
                    statuses.push(status);
                }
            }
            Err(error) => {
                let mut rollback_failures = Vec::new();
                for actor_id in started_actor_ids.iter().rev() {
                    if let Err(rollback) = apply(home, group, actor_id, "actor.stop") {
                        rollback_failures.push(format!("{actor_id}: {}", rollback.message));
                    }
                }
                return if rollback_failures.is_empty() {
                    Err(error)
                } else {
                    Err(OpError::new(
                        "rollback_failed",
                        format!(
                            "{}; failed to stop newly started Actors: {}",
                            error.message,
                            rollback_failures.join("; ")
                        ),
                    ))
                };
            }
        }
    }
    Ok(statuses)
}

pub(super) fn actor_is_running(group: &GroupDoc, actor: &Actor) -> bool {
    super::local_headless::registered_running(&group.group_id, &actor.id).unwrap_or_else(|| {
        super::deepseek_runtime::running(&group.group_id, &actor.id)
            || status(&group.group_id, &actor.id).is_some_and(|status| status.running)
    })
}

pub(crate) fn stop_all() -> Result<Vec<SessionStatus>, cccc_runtime::RuntimeError> {
    super::deepseek_runtime::stop_all();
    cccc_runtime::stop_all()
}

pub fn stop_group(home: &HomeLayout, group: &GroupDoc) -> Result<Vec<SessionStatus>, OpError> {
    if cccc_core::settings::detach_claude_on_exit(home).map_err(OpError::io)? {
        let mut statuses = Vec::new();
        for actor in &group.actors {
            if let Some(status) = stop_registered(home, group, &actor.id, true)? {
                statuses.push(status);
            }
        }
        super::local_headless::stop_group(&group.group_id).map_err(OpError::io)?;
        super::deepseek_runtime::stop_group(&group.group_id);
        return Ok(statuses);
    }
    super::local_headless::stop_group(&group.group_id).map_err(OpError::io)?;
    super::deepseek_runtime::stop_group(&group.group_id);
    let mut stopped = Vec::new();
    for actor in &group.actors {
        if let Some(status) = stop(group, &actor.id)? {
            stopped.push(status);
        }
    }
    Ok(stopped)
}

pub(super) fn working_directory(group: &GroupDoc, actor: &Actor) -> Result<PathBuf, OpError> {
    let wanted = if actor.default_scope_key.is_empty() {
        &group.active_scope_key
    } else {
        &actor.default_scope_key
    };
    if wanted.is_empty() {
        return Err(OpError::new(
            "missing_project_root",
            "missing project root for group (no active scope)",
        ));
    }
    let scope = cccc_core::group_scope::resolve_attached_scope(group, wanted).ok_or_else(|| {
        OpError::new(
            "scope_not_attached",
            format!("scope not attached: {wanted}"),
        )
    })?;
    let path = PathBuf::from(&scope.url);
    if !path.is_dir() {
        return Err(OpError::new(
            "invalid_project_root",
            format!("project root path does not exist: {}", path.display()),
        ));
    }
    Ok(path)
}

fn runtime_error(error: cccc_runtime::RuntimeError) -> OpError {
    OpError::new("runtime_error", error.to_string())
}

/// Resolve teardown from the original receipt, never the replacement workspace.
pub(crate) fn saved_claude_binding(
    home: &HomeLayout,
    group: &GroupDoc,
    actor: &Actor,
) -> Result<Option<super::runtime_session::claude_ownership::Ownership>, OpError> {
    use super::runtime_session::claude_ownership as ownership;
    if let Some(owner) = ownership::load(home, &group.group_id, actor).map_err(OpError::io)? {
        return Ok(Some(owner));
    }
    let Some(id) =
        ownership::saved_session(home, &group.group_id, &actor.id).map_err(OpError::io)?
    else {
        return Ok(None);
    };
    let saved = super::runtime_session::snapshot(home, &group.group_id, &actor.id)
        .map_err(OpError::io)?
        .ok_or_else(|| OpError::new("not_found", "Claude receipt disappeared"))?;
    let cwd = PathBuf::from(
        saved
            .get("workspace_path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default(),
    );
    let resolved = environment::resolve_launch_actor(home, group, actor)?;
    let env = environment::launch_env(home, group, &resolved);
    // Legacy receipts do not store the config path. Require their original
    // identity before deriving it from effective configuration.
    let identity = cccc_core::codex_voice_settings::ResolvedAgentRuntime {
        runtime: ActorRuntime::Claude,
        runtime_mode: resolved.runtime_mode,
        command: resolved.command.clone(),
        environment: env.clone(),
    }
    .identity_fingerprint_at(&cwd)
    .map_err(OpError::io)?;
    if saved
        .get("identity_fingerprint")
        .and_then(serde_json::Value::as_str)
        != Some(&identity)
    {
        return Err(OpError::new(
            CLAUDE_RESUME_FAILED,
            "Cannot resolve original Claude provider configuration from legacy receipt; restore its configuration before teardown",
        ));
    }
    let config = super::codex_voice_analyst::claude_config_dir(&env).map_err(OpError::io)?;
    Ok(Some(ownership::Ownership {
        group_id: group.group_id.clone(),
        actor_id: actor.id.clone(),
        actor_created_at: actor.created_at.clone(),
        session_id: id,
        config_dir: config,
        workspace: cwd,
        uncertain: false,
        attempt_id: String::new(),
    }))
}
