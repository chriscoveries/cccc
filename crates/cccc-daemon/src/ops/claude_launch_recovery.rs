//! Explicit operator reconciliation; listing jobs never establishes ownership.
use cccc_contracts::DaemonRequest;
use cccc_core::{GroupDoc, HomeLayout};
use serde_json::json;

use super::runtime_session::claude_ownership::{self, Ownership};
use crate::dispatch::{OpError, OpResult, object, required_arg, store, string_arg};

fn require_user(request: &DaemonRequest) -> Result<(), OpError> {
    if string_arg(request, "by").as_deref() != Some("user") {
        return Err(OpError::new(
            "permission_denied",
            "Claude launch reconciliation requires by=user",
        ));
    }
    Ok(())
}

fn owned(
    home: &HomeLayout,
    request: &DaemonRequest,
) -> Result<(GroupDoc, cccc_contracts::Actor, Ownership), OpError> {
    require_user(request)?;
    let group = store(home)?
        .load(&required_arg(request, "group_id")?)
        .map_err(OpError::not_found)?;
    let id = required_arg(request, "actor_id")?;
    let actor = group
        .actors
        .iter()
        .find(|actor| actor.id == id)
        .cloned()
        .ok_or_else(|| OpError::new("actor_not_found", "Claude reconciliation actor is missing"))?;
    let owner = claude_ownership::load(home, &group.group_id, &actor)
        .map_err(OpError::io)?
        .ok_or_else(|| {
            OpError::new(
                "no_uncertain_launch",
                "This actor has no Claude launch fence to reconcile",
            )
        })?;
    if !owner.uncertain {
        return Err(OpError::new(
            "no_uncertain_launch",
            "This actor has no uncertain Claude launch",
        ));
    }
    Ok((group, actor, owner))
}

pub(super) fn inspect(home: &HomeLayout, request: &DaemonRequest) -> OpResult {
    let (group, actor, owner) = owned(home, request)?;
    let (jobs, error) = match super::local_headless::inspect_claude_jobs(&owner.config_dir) {
        Ok(jobs) => (Some(jobs), None),
        Err(error) => (None, Some(error.to_string())),
    };
    object(json!({
        "group_id":group.group_id, "actor_id":actor.id,
        "actor_generation":actor.generation, "actor_created_at":actor.created_at,
        "config_dir":owner.config_dir, "attempt_id":owner.attempt_id,
        "session_id":owner.session_id, "resettable":owner.session_id.is_empty(),
        "jobs":jobs, "inspection_error":error,
    }))
}

pub(super) fn reset(home: &HomeLayout, request: &DaemonRequest) -> OpResult {
    require_user(request)?;
    let key = (
        required_arg(request, "group_id")?,
        required_arg(request, "actor_id")?,
    );
    // Dispatch owns the group lock first. Try the same per-actor guard used by
    // delivery-worker starts; never wait for a launch while holding the group.
    // Keep the guard across the ownership read, validation and deletion. No
    // provider RPC or further dispatch-lock acquisition occurs in this section.
    let _start = super::local_headless::StartGuard::try_acquire(&key)
        .map_err(OpError::io)?
        .ok_or_else(|| {
            OpError::new(
                super::actor_runtime::RUNTIME_BUSY,
                format!(
                    "Claude launch is in progress; retry reconciliation after it finishes. {}",
                    claude_ownership::guidance(&key.0, &key.1)
                ),
            )
        })?;
    let (group, actor, owner) = owned(home, request)?;
    if request
        .args
        .get("acknowledge")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return Err(OpError::new(
            "acknowledgment_required",
            claude_ownership::guidance(&group.group_id, &actor.id),
        ));
    }
    // Empty legacy attempt IDs are valid, but must be supplied explicitly.
    if string_arg(request, "actor_generation").as_deref() != Some(actor.generation.as_str())
        || required_arg(request, "actor_created_at")? != actor.created_at
        || required_arg(request, "config_dir")? != owner.config_dir.to_string_lossy()
        || string_arg(request, "attempt_id").as_deref() != Some(owner.attempt_id.as_str())
    {
        return Err(OpError::new(
            "stale_claude_launch",
            format!(
                "Claude actor generation, original configuration, or launch attempt changed. {}",
                claude_ownership::guidance(&group.group_id, &actor.id)
            ),
        ));
    }
    if !owner.session_id.is_empty() {
        return Err(OpError::new(
            "identified_claude_launch",
            claude_ownership::guidance(&group.group_id, &actor.id),
        ));
    }
    claude_ownership::remove(home, &group.group_id, &actor.id).map_err(OpError::io)?;
    object(json!({"group_id":group.group_id,"actor_id":actor.id,"cleared":true,"jobs_stopped":0}))
}
