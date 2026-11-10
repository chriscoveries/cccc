//! Scheduled, agent-bound pull delivery. Polling never replays unfinished work.
use super::operation::{Operation, Policy::{GlobalWrite, Read, Write}};
use crate::dispatch::{OpError, OpResult, object, required_arg, string_arg};
use cccc_contracts::{Actor, ActorRuntime, DaemonRequest, GroupState, RunnerKind};
use cccc_core::{GroupDoc, GroupStore, HomeLayout, web_model_connectors as store};
use serde_json::{Value, json};
use store::hark as private;

#[cfg(test)]
mod tests;

pub(super) fn resolve_operation(request: &DaemonRequest) -> Option<Operation> {
    Some(match request.op.as_str() {
        "hark_connector_configure" => Operation::new(GlobalWrite, configure),
        "hark_connector_revoke" => Operation::new(GlobalWrite, revoke),
        "hark_pull_status" => Operation::new(Read, status),
        "hark_work_claim" => Operation::new(Write, claim),
        "hark_work_resolve" => Operation::new(Write, resolve),
        _ => return None,
    })
}

fn state_error(error: std::io::Error) -> OpError {
    let code = error.to_string();
    if matches!(code.as_str(), "hark_connector_exists" | "hark_connector_required" | "hark_task_expired" | "hark_binding_changed" | "connector_unavailable") {
        return OpError::new(code, "The Hark connector is unavailable or changed");
    }
    OpError::new("hark_state_unavailable", "Hark private state could not be read or saved")
}

fn user(r: &DaemonRequest) -> Result<(), OpError> {
    if string_arg(r, "by").as_deref() == Some("user") {
        Ok(())
    } else {
        Err(OpError::new("permission_denied", "Hark connection controls require the user"))
    }
}

fn group_actor(home: &HomeLayout, r: &DaemonRequest) -> Result<(GroupDoc, Actor), OpError> {
    let g = GroupStore::new(home.clone()).map_err(OpError::io)?
        .load(&required_arg(r, "group_id")?).map_err(OpError::not_found)?;
    let a = g.actors.iter()
        .find(|a| Some(a.id.as_str()) == r.args.get("actor_id").and_then(Value::as_str))
        .filter(|a| a.runtime == ActorRuntime::Hark && a.runner == RunnerKind::Headless)
        .cloned().ok_or_else(|| OpError::new("invalid_actor_runtime", "A Hark headless Actor is required"))?;
    Ok((g, a))
}

fn active(g: &GroupDoc, a: &Actor) -> bool {
    a.enabled && g.running && !matches!(g.state, GroupState::Paused | GroupState::Stopped)
        && super::runtime_state::pull_admitted(g, a)
}

fn connector(home: &HomeLayout, g: &GroupDoc, a: &Actor) -> Result<Option<Value>, OpError> {
    Ok(store::load(home).map_err(state_error)?.into_iter().find(|c| {
        c["kind"] == "hark_connector" && c["provider"] == "hark" && c["revoked"] != true
            && store::binding_for_actor(c, &g.group_id, &a.id, &cccc_core::actors::generation_identity(a)).is_some()
    }))
}

fn configure(home: &HomeLayout, r: &DaemonRequest) -> OpResult {
    user(r)?;
    let (g, a) = group_actor(home, r)?;
    super::actor_switch::refuse_pending(home, &g.group_id, &a.id)?;
    let rotate = match r.args.get("rotate") {
        None => false,
        Some(v) => v.as_bool().ok_or_else(|| OpError::new("invalid_argument", "rotate must be a boolean"))?,
    };
    // Rotation retires remote authority but deliberately retains the canonical
    // unfinished turn. Only explicit resolution may clear that turn.
    object(private::configure(home, &g.group_id, &a.id, &cccc_core::actors::generation_identity(&a), rotate).map_err(state_error)?)
}

fn revoke(home: &HomeLayout, r: &DaemonRequest) -> OpResult {
    user(r)?;
    let (g, a) = group_actor(home, r)?;
    let Some(c) = connector(home, &g, &a)? else {
        return object(json!({"revoked":false}));
    };
    if r.args.get("connector_id").is_some_and(|id| id != &c["connector_id"]) {
        return Err(OpError::new("hark_binding_changed", "The Hark connector changed"));
    }
    object(json!({"revoked":store::revoke(home, c["connector_id"].as_str().unwrap_or("")).map_err(state_error)?}))
}

fn status(home: &HomeLayout, r: &DaemonRequest) -> OpResult {
    let (g, a) = group_actor(home, r)?;
    let c = connector(home, &g, &a)?.unwrap_or(Value::Null);
    let s = private::state(&c, &g.group_id, &a.id);
    let runtime = super::runtime_state::actor_state(home, &g.group_id, &a.id)?;
    let unfinished = runtime["active_turn_id"].as_str().is_some_and(|id| !id.is_empty());
    let pending = super::runtime_delivery::pending_sources(home, &g, &a, 10_000)?.len();
    let phase = if unfinished {
        if active(&g, &a) && s["execution"]["state"] == "active" && s["execution"]["turn_id"] == runtime["active_turn_id"] {
            "working"
        } else { "interrupted" }
    } else if c.is_null() { "unconfigured" }
    else if !active(&g, &a) { "stopped" }
    else if pending > 0 { "pending" }
    else { "idle" };
    object(json!({"group_id":g.group_id,"actor_id":a.id,"connector_id":c["connector_id"],
        "configured":!c.is_null(),"enabled":a.enabled,"phase":phase,"pending_count":pending,
        "last_poll_at":s["last_poll_at"],"last_poll_status":s["last_poll_status"],
        "last_claim_at":s["last_claim_at"],"reason_code":s["reason_code"],
        "active_turn_id":runtime["active_turn_id"],"active_event_ids":runtime["active_event_ids"],
        "active_turn_elapsed_seconds":super::runtime_state::active_turn_elapsed_seconds(&runtime,&s["execution"],chrono::Utc::now()),
        "last_execution":s["last_execution"]}))
}

fn claim(home: &HomeLayout, r: &DaemonRequest) -> OpResult {
    let (g, a) = group_actor(home, r)?;
    let b = r.args.get("_cccc_web_binding")
        .filter(|b| b["transport"] == "hark_pull" && b["hark_stage"] == "claim")
        .ok_or_else(|| OpError::new("hark_connector_required", "Use this Actor's Hark connector to poll"))?;
    if string_arg(r, "by").as_deref() != Some(&a.id) {
        return Err(OpError::new("permission_denied", "Poll as the bound Hark Actor"));
    }
    let mut pulled = r.clone();
    pulled.args.insert("transport".into(), json!("web_model_pull"));
    // Group write permit precedes the connector store lock. Duplicate polls are
    // serialized and canonical ownership wins after a partial/private write.
    let result = private::change(home, b["connector_id"].as_str().unwrap_or(""), &g.group_id, &a.id, |c, s| {
        let route = store::binding_for_actor(c, &g.group_id, &a.id, &cccc_core::actors::generation_identity(&a))
            .ok_or_else(|| std::io::Error::other("hark_binding_changed"))?;
        if route["revision"] != b["revision"] || s["epoch"] != b["hark_epoch"] {
            return Err(std::io::Error::other("hark_task_expired"));
        }
        s["last_poll_at"] = json!(cccc_contracts::utc_now());
        let runtime = super::runtime_state::actor_state(home, &g.group_id, &a.id)
            .map_err(|_| std::io::Error::other("runtime_unavailable"))?;
        let unfinished = runtime["active_turn_id"].as_str().is_some_and(|id| !id.is_empty())
            || s["execution"]["state"] == "active";
        let mut result = if unfinished {
            json!({"status":"turn_in_progress","turn":null,"active_turn_id":runtime["active_turn_id"],
                "event_ids":runtime["active_event_ids"],"reason_code":"hark_turn_unfinished",
                "instructions":"An unfinished turn is visible in Hark status. Finish it with its original execution credential, or have the owner explicitly resolve it. Never replay it from a poll."})
        } else if !active(&g, &a) {
            json!({"status":"stopped","turn":null,"reason_code":"hark_actor_stopped"})
        } else {
            match super::runtime_state::wait_next_turn(home, &pulled) {
                Err(error) => {
                    s["last_poll_status"] = json!("failed");
                    s["reason_code"] = json!(error.code);
                    return Ok(Err(error));
                }
                Ok(result) => Value::Object(result),
            }
        };
        if result["status"] == "work_available" {
            s["execution"] = json!({"turn_id":result["turn"]["turn_id"],"event_ids":result["turn"]["event_ids"],
                "state":"active","claimed_at":result["turn"]["created_at"]});
            s["last_claim_at"] = result["turn"]["created_at"].clone();
            result["actor_token"] = json!(private::token(c, b, s, result["turn"]["turn_id"].as_str().unwrap_or(""))?);
            result["reason_code"] = json!("hark_turn_claimed");
        } else if result["status"] == "idle" {
            result["reason_code"] = json!("hark_no_work");
            // The hosted scheduler decides cadence; a poll never long-holds.
            result["suggested_retry_after_ms"] = json!(600_000);
        }
        s["last_poll_status"] = result["status"].clone();
        s["reason_code"] = result["reason_code"].clone();
        Ok(Ok(result))
    }).map_err(state_error)??;
    let result = object(result)?;
    if let Some(turn_id) = result.get("turn").and_then(|t| t.get("turn_id")).and_then(Value::as_str) {
        super::runtime_state::confirm_pulled_instruction_notice(home, &g.group_id, &a.id, turn_id);
    }
    Ok(result)
}

pub(super) fn completed(home: &HomeLayout, g: &GroupDoc, actor: &str, completion: &super::runtime_completion::Completion) -> Result<(), OpError> {
    let Some(a) = g.actors.iter().find(|a| a.id == actor && a.runtime == ActorRuntime::Hark) else { return Ok(()); };
    let Some(c) = connector(home, g, a)? else { return Ok(()); };
    private::change(home, c["connector_id"].as_str().unwrap_or(""), &g.group_id, actor, |_, s| {
        if s["execution"]["turn_id"] == completion.turn_id {
            let mut last = s["execution"].clone();
            last["state"] = json!("completed");
            last["status"] = json!(completion.status);
            last["completed_at"] = json!(cccc_contracts::utc_now());
            s["last_execution"] = last;
            s["execution"] = Value::Null;
        }
        Ok(())
    }).map_err(state_error)
}

fn resolve(home: &HomeLayout, r: &DaemonRequest) -> OpResult {
    user(r)?;
    let (g, a) = group_actor(home, r)?;
    let runtime = super::runtime_state::actor_state(home, &g.group_id, &a.id)?;
    let turn = required_arg(r, "turn_id")?;
    let ids = r.args.get("event_ids").and_then(Value::as_array)
        .filter(|ids| !ids.is_empty() && ids.len() <= 20 && ids.iter().all(Value::is_string))
        .ok_or_else(|| OpError::new("invalid_event_ids", "Exact active event_ids are required"))?;
    if runtime["active_turn_id"] != turn || runtime["active_event_ids"] != json!(ids) {
        return Err(OpError::new("stale_turn", "The unfinished Hark turn changed"));
    }
    private::invalidate(home, &g.group_id, &a.id).map_err(state_error)?;
    let ids = ids.iter().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>();
    let completion = super::runtime_completion::committed(home, &g.group_id, &a.id, &turn, &ids)?
        .unwrap_or(super::runtime_completion::Completion { turn_id:turn.clone(),event_ids:ids,
            status:"cancelled".into(),delivery_id:format!("runtime:{turn}") });
    for event in &completion.event_ids {
        if let Some((state, transport)) = super::runtime_delivery::latest_state(home, &g.group_id, &a.id, event)?
            && state == "claimed" {
            super::runtime_delivery::append_state(home, &g.group_id, &a.id, &a.created_at, event, &transport,
                super::runtime_delivery::DeliveryOutcome::Ambiguous("Unfinished Hark turn explicitly resolved; do not replay"))?;
        }
    }
    let receipt = super::runtime_completion::append(home, &g.group_id, &a.id, &completion)?;
    super::runtime_state::finish_completion(home, &g, &a.id, &completion, receipt, r)
}

pub(super) fn invalidate(home: &HomeLayout, g: &GroupDoc, actor: &str) -> Result<(), OpError> {
    if g.actors.iter().any(|a| a.id == actor && a.runtime == ActorRuntime::Hark) {
        private::invalidate(home, &g.group_id, actor).map_err(state_error)?;
    }
    Ok(())
}

pub(super) fn invalidate_group(home: &HomeLayout, g: &GroupDoc) -> Result<(), OpError> {
    for a in &g.actors { invalidate(home, g, &a.id)?; }
    Ok(())
}
