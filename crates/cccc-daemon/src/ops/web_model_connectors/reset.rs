//! Attended retirement of a settled browser route; never completion or replay.
use super::*;
use cccc_contracts::{ActorRuntime, Event};
use cccc_core::ledger;
use serde_json::Value;
use sha2::{Digest, Sha256};

fn digest(value: &Value) -> String {
    format!("{:x}", Sha256::digest(value.to_string().as_bytes()))
}

fn audit_id(kind: &str, group: &str, actor: &str, snapshot: &str) -> String {
    digest(&json!([kind, group, actor, snapshot]))[..32].to_owned()
}

fn routes(home: &HomeLayout, group: &str, actor: &str) -> Result<Value, OpError> {
    let mut entries = Vec::new();
    for connector in store::load(home).map_err(OpError::io)? {
        if connector["dots"]["group_id"] == group && connector["dots"]["actor_id"] == actor {
            return Err(OpError::new(
                "reset_unsafe",
                "A Dots route requires its own native control",
            ));
        }
        for collection in ["bindings", "pairings", "grok_webhooks"] {
            if let Some(values) = connector[collection].as_object() {
                for (key, value) in values {
                    if value["group_id"] == group && value["actor_id"] == actor {
                        if collection == "grok_webhooks" || value["transport"] == "mcp_events" {
                            return Err(OpError::new(
                                "reset_unsafe",
                                "Non-browser authority is present; reset refused",
                            ));
                        }
                        entries.push(json!([connector["connector_id"], collection, key, value]));
                    }
                }
            }
        }
    }
    Ok(json!(entries))
}

pub(super) fn execute(home: &HomeLayout, request: &DaemonRequest) -> OpResult {
    execute_with_hook(home, request, |_| Ok(()))
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Intent,
    AuthorityRetired,
    TargetResolved,
}

fn execute_with_hook(
    home: &HomeLayout,
    request: &DaemonRequest,
    mut after: impl FnMut(Phase) -> Result<(), OpError>,
) -> OpResult {
    let group_id = required_arg(request, "group_id")?;
    let actor_id = required_arg(request, "actor_id")?;
    let reason = required_arg(request, "reason")?;
    if reason.trim().is_empty() || reason.len() > 512 || reason.chars().any(char::is_control) {
        return Err(OpError::new(
            "invalid_argument",
            "reason must be 1..512 bytes without control characters",
        ));
    }
    let apply = match request.args.get("apply") {
        None => false,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| OpError::new("invalid_argument", "apply must be boolean"))?,
    };
    let groups = GroupStore::new(home.clone()).map_err(OpError::io)?;
    let ledger_path = groups.ledger_path(&group_id).map_err(OpError::io)?;
    let expected = string_arg(request, "expected_snapshot").unwrap_or_default();
    // A lost response must return its old outcome without touching a successor.
    let outcome_id = audit_id("outcome", &group_id, &actor_id, &expected);
    if apply
        && !expected.is_empty()
        && let Some(outcome) = ledger::find_event(&ledger_path, &outcome_id).map_err(OpError::io)?
    {
        if outcome.kind != "web_model.delivery_reset.completed"
            || outcome.data.get("reason") != Some(&json!(reason))
        {
            return Err(OpError::new("reset_conflict", "reset receipt differs"));
        }
        return object(json!({"reset":outcome.data,"already_applied":true}));
    }
    let group = groups.load(&group_id).map_err(OpError::not_found)?;
    let actor = group
        .actors
        .iter()
        .find(|a| a.id == actor_id)
        .ok_or_else(|| OpError::new("actor_not_found", "Actor not found"))?;
    if actor.runtime != ActorRuntime::WebModel
        || actor.runner != RunnerKind::Headless
        || !actor.external_session_id.is_empty()
    {
        return Err(OpError::new(
            "invalid_actor_runtime",
            "reset supports browser Web Model Actors only, never Dots",
        ));
    }
    super::super::actor_switch::refuse_pending(home, &group_id, &actor_id)?;
    // The same cross-process exclusion spans every browser attempt and its final write.
    let _browser = super::super::actor_runtime::hold_browser_work(home, &group_id, &actor_id)?;
    let quiescent = !actor.enabled
        && super::super::actor_runtime::delivery_owner(&group_id, &actor_id)
            .map_err(OpError::io)?
            .is_none();
    let runtime = super::super::runtime_state::actor_state(home, &group_id, &actor_id)?;
    let targets = integration_state::group_get(&groups, &group_id, "web_model_browser_targets")
        .map_err(OpError::io)?;
    let target = targets
        .get(&actor_id)
        .filter(|v| v.is_object())
        .ok_or_else(|| OpError::new("missing_delivery", "No saved browser delivery"))?;
    // The projection was committed, but its outcome append/response was lost.
    // Complete only that prior audit, even if a new pairing now exists.
    if apply && !expected.is_empty() && target["last_delivery_reset"]["snapshot"] == expected {
        let result = &target["last_delivery_reset"];
        if result["reason"] != reason {
            return Err(OpError::new("reset_conflict", "reset reason differs"));
        }
        let mut outcome = Event::new("web_model.delivery_reset.completed", &group_id);
        outcome.id = outcome_id;
        outcome.by = "user".into();
        outcome.data = result
            .as_object()
            .cloned()
            .ok_or_else(|| OpError::new("reset_conflict", "invalid reset marker"))?;
        ledger::append(&ledger_path, &outcome).map_err(OpError::io)?;
        return object(json!({"reset":result,"already_applied":true}));
    }
    let field = |name: &str| {
        target[name]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| OpError::new("invalid_delivery", format!("missing {name}")))
    };
    let delivery = field("last_delivery_id")?;
    let turn = field("last_delivery_turn_id")?;
    let events: Vec<String> = serde_json::from_value(target["last_delivery_event_ids"].clone())
        .map_err(|_| OpError::new("invalid_delivery", "event IDs must be strings"))?;
    if events.is_empty() || events.len() > 20 || events.iter().any(String::is_empty) {
        return Err(OpError::new(
            "invalid_delivery",
            "exact source event IDs are required",
        ));
    }
    let completion =
        super::super::runtime_completion::committed(home, &group_id, &actor_id, turn, &events)?;
    if completion
        .as_ref()
        .is_some_and(|c| c.delivery_id != delivery)
    {
        return Err(OpError::new(
            "completion_conflict",
            "saved delivery differs from completion",
        ));
    }
    let inactive = runtime.is_null()
        || (runtime.is_object()
            && runtime["active_turn_id"].as_str() == Some("")
            && runtime["status"] == "waiting"
            && runtime["active_event_ids"]
                .as_array()
                .is_some_and(Vec::is_empty));
    let committed = completion.as_ref().map(|c| {
        json!({"turn_id":c.turn_id,
        "delivery_id":c.delivery_id,"event_ids":c.event_ids,"status":c.status})
    });
    let identity = digest(&json!({"group_id":group_id,"actor":actor,"target":target,
        "runtime":runtime,"completion":committed,"reason":reason}));
    let route_state = routes(home, &group_id, &actor_id)?;
    let route_hash = digest(&route_state);
    let snapshot = digest(&json!([identity, route_hash]));
    let mut result = json!({
        "group_id":group_id,"actor_id":actor_id,"reason":reason,
        "snapshot":snapshot,"generation":cccc_core::actors::generation_identity(actor),
        "delivery_id":delivery,"turn_id":turn,"event_ids":events,
        "saved_status":target["last_delivery_status"],
        "route_records_to_retire":route_state.as_array().map(Vec::len),
        "completion_status":completion.as_ref().map(|c| c.status.as_str()),
        "actor_stopped":quiescent,"canonical_inactive":inactive,
        "can_apply":quiescent && inactive && completion.is_some(),
        "action":"retire_settled_conversation_authority",
        "warning":"No provider execution is undone. Source records and completion receipts are preserved. Unseen/unfinished work is never acknowledged or requeued. Stop the Actor before applying; use a fresh chat when chat-local tool loss is suspected."
    });
    if !apply {
        return object(json!({"reset":result,"dry_run":true}));
    }
    if !quiescent || !inactive || completion.is_none() {
        return Err(OpError::new(
            "reset_unsafe",
            "Requires stopped browser ownership, no active turn and an exact committed completion; unfinished/unknown work is held",
        ));
    }
    if expected.is_empty() {
        return Err(OpError::new(
            "missing_snapshot",
            "Preview and supply expected_snapshot",
        ));
    }
    let intent_id = audit_id("intent", &group_id, &actor_id, &expected);
    let previous = ledger::find_event(&ledger_path, &intent_id).map_err(OpError::io)?;
    if let Some(intent) = &previous {
        if intent.kind != "web_model.delivery_reset.requested"
            || intent.data.get("identity") != Some(&json!(identity))
            || (route_hash != intent.data["routes"].as_str().unwrap_or("")
                && route_state != json!([]))
        {
            return Err(OpError::new(
                "reset_conflict",
                "Actor, delivery or route changed since the reset intent",
            ));
        }
    } else if expected != snapshot {
        return Err(OpError::new(
            "reset_conflict",
            "Actor, delivery or route changed; preview again",
        ));
    }
    if previous.is_none() {
        let mut intent = Event::new("web_model.delivery_reset.requested", &group_id);
        intent.id = intent_id;
        intent.by = "user".into();
        intent.data =
            json!({"identity":identity,"routes":route_hash,"reason":reason,"reset":result})
                .as_object()
                .cloned()
                .expect("reset intent");
        ledger::append(&ledger_path, &intent).map_err(OpError::io)?;
    }
    after(Phase::Intent)?;
    // Authority first: a crash here leaves a stopped, unpaired Actor with all
    // source/receipt fences intact. Retrying the same intent accepts only an
    // unchanged target and the original route set or its empty retired set.
    store::retire_actor(home, &group_id, &actor_id).map_err(OpError::io)?;
    after(Phase::AuthorityRetired)?;
    result["snapshot"] = json!(expected);
    result["applied"] = json!(true);
    integration_state::group_update(&groups, &group_id, "web_model_browser_targets", |targets| {
        if targets.get(&actor_id) != Some(target) {
            return Err(std::io::Error::other("browser target changed during reset"));
        }
        let current = targets.get_mut(&actor_id).expect("target checked");
        current["last_delivery_status"] = json!("resolved");
        current["state"] = json!("pairing_required");
        current["next_delivery"] = json!("blocked");
        current["last_delivery_reset"] = result.clone();
        Ok(())
    })
    .map_err(OpError::io)?;
    after(Phase::TargetResolved)?;
    let mut outcome = Event::new("web_model.delivery_reset.completed", &group_id);
    outcome.id = outcome_id;
    outcome.by = "user".into();
    outcome.data = result.as_object().cloned().expect("reset outcome");
    ledger::append(&ledger_path, &outcome).map_err(OpError::io)?;
    object(json!({"reset":result,"already_applied":false}))
}

#[cfg(test)]
mod tests;
