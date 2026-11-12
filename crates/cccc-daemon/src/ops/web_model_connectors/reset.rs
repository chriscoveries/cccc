//! Fenced browser-authority retirement or an idle, completed restart. Never replay.
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
    Restarted,
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
    let automatic = match request.args.get("automatic") {
        None => false,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| OpError::new("invalid_argument", "automatic must be boolean"))?,
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
            || outcome.data.get("automatic") != Some(&json!(automatic))
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
    let _delivery = super::super::actor_delivery::fence_idle_actor(&group_id, &actor_id)?;
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
        if result["reason"] != reason || result["automatic"] != automatic {
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
    if events.is_empty()
        || events.len() > 20
        || events.iter().any(String::is_empty)
        || events
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != events.len()
    {
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
    // Existing terminal delivery facts remain enforceable after re-pairing:
    // normal pending_sources/claim exclude accepted and ambiguous sources.
    let source_fenced = events.iter().all(|id| {
        let source = ledger::find_event(&ledger_path, id).ok().flatten();
        source.is_some_and(|event| {
            matches!(event.kind.as_str(), "chat.message" | "system.notify")
                && event.by != actor_id
                && cccc_core::inbox::is_for_actor(&group, &event, &actor_id)
                && chrono::DateTime::parse_from_rfc3339(&event.ts)
                    .ok()
                    .zip(chrono::DateTime::parse_from_rfc3339(&actor.created_at).ok())
                    .is_some_and(|(at, born)| at >= born)
        }) && super::super::runtime_delivery::latest_state(home, &group_id, &actor_id, id)
            .ok()
            .flatten()
            .is_some_and(|(state, _)| matches!(state.as_str(), "accepted" | "ambiguous"))
    });
    let committed = completion.as_ref().map(|c| {
        json!({"turn_id":c.turn_id,
        "delivery_id":c.delivery_id,"event_ids":c.event_ids,"status":c.status})
    });
    let identity = digest(&json!({"group_id":group_id,"actor":actor,"target":target,
        "runtime":runtime,"completion":committed,"reason":reason,"automatic":automatic,
        "group_state":group.state,"group_running":group.running}));
    let route_state = routes(home, &group_id, &actor_id)?;
    // Only non-secret identity metadata is shown to the operator. In
    // particular, no pairing code, session key, token or conversation URL.
    let authorities: Vec<Value> = route_state
        .as_array()
        .into_iter()
        .flatten()
        .map(|entry| {
            let value = &entry[3];
            json!({"connector_id":entry[0],"collection":entry[1],"record_key":entry[2],
            "revision":value["revision"],"generation":value["generation"],
            "pairing_id":value["pairing_id"],"state":value["state"]})
        })
        .collect();
    let route_hash = digest(&route_state);
    let routes = route_state.as_array().expect("route array");
    let unbound = !routes
        .iter()
        .any(|r| r[1] == "bindings" && r[3]["state"] == "bound");
    let pairing_idle = routes.iter().filter(|r| r[1] == "pairings").all(|r| {
        matches!(
            r[3]["state"].as_str(),
            Some("failed" | "expired" | "cancelled")
        )
    });
    let stale = target["last_delivery_started_at"]
        .as_str()
        .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
        .is_some_and(|at| chrono::Utc::now().signed_duration_since(at).num_seconds() >= 1800)
        && (matches!(
            target["last_delivery_status"].as_str(),
            Some("pending" | "submitting" | "submission_ambiguous" | "completion_ambiguous")
        ) || (target["last_delivery_status"] == "submitted" && unbound));
    // A current failed automatic setup is positive obstruction evidence. A
    // manually cancelled/expired pairing or deliberately stopped member is
    // never an automatic restart invitation.
    let obstruction = unbound
        && routes.iter().any(|r| {
            r[1] == "pairings"
                && r[3]["automatic"] == true
                && r[3]["state"] == "failed"
                && r[3]["error_code"] == "pairing_timeout"
                && r[3]["generation"] == cccc_core::actors::generation_identity(actor)
        });
    let can_restart = group.running
        && group.state == cccc_contracts::GroupState::Active
        && actor.enabled
        && inactive
        && completion.is_some()
        && source_fenced
        && stale
        && pairing_idle
        && obstruction
        && actor.profile_id.is_empty()
        && super::super::actor_runtime::delivery_owner(&group_id, &actor_id)
            .map_err(OpError::io)?
            .is_none();
    let snapshot = digest(&json!([identity, route_hash]));
    let mut result = json!({
        "group_id":group_id,"actor_id":actor_id,"reason":reason,
        "snapshot":snapshot,"generation":cccc_core::actors::generation_identity(actor),
        "delivery_id":delivery,"turn_id":turn,"event_ids":events,
        "saved_status":target["last_delivery_status"],
        "route_records_to_retire":if automatic { 0 } else { authorities.len() },
        "authorities":authorities,
        "completion_status":completion.as_ref().map(|c| c.status.as_str()),
        "source_fenced":source_fenced,"automatic":automatic,
        "actor_stopped":quiescent,"canonical_inactive":inactive,
        "can_apply":if automatic { can_restart } else { quiescent && inactive && source_fenced },
        "action":if automatic { "restart_browser_actor" } else if completion.is_some() { "retire_settled_conversation_authority" } else { "abandon_browser_authority" },
        "warning":"No provider execution is undone. Source records and completion receipts are preserved. Unseen/unfinished work is never acknowledged or requeued. Stop the Actor before applying; use a fresh chat when chat-local tool loss is suspected."
    });
    if !apply {
        return object(json!({"reset":result,"dry_run":true}));
    }
    if result["can_apply"] != true {
        return Err(OpError::new(
            "reset_unsafe",
            "Requires quiescent ownership and exact fenced sources; automatic restart additionally requires committed completion and a stale-authority obstruction",
        ));
    }
    if expected.is_empty() {
        return Err(OpError::new(
            "missing_snapshot",
            "Preview and supply expected_snapshot",
        ));
    }
    let intent_key = if automatic {
        digest(&json!([
            cccc_core::actors::generation_identity(actor),
            delivery,
            turn,
            events
        ]))
    } else {
        expected.clone()
    };
    let intent_id = audit_id("intent", &group_id, &actor_id, &intent_key);
    let previous = ledger::find_event(&ledger_path, &intent_id).map_err(OpError::io)?;
    if automatic && previous.is_some() {
        return Err(OpError::new(
            "reset_outcome_unknown",
            "A prior restart intent has no outcome; do not repeat the action",
        ));
    }
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
    if automatic {
        // The native global write permit and both ownership guards still
        // span the check and the existing lifecycle operation. No new-session.
        super::super::actors::lifecycle(home, request, "actor.restart")?;
        after(Phase::Restarted)?;
        result["applied"] = json!(true);
        let mut outcome = Event::new("web_model.delivery_reset.completed", &group_id);
        outcome.id = outcome_id;
        outcome.by = "user".into();
        outcome.data = result.as_object().cloned().expect("restart outcome");
        ledger::append(&ledger_path, &outcome).map_err(OpError::io)?;
        return object(json!({"reset":result,"already_applied":false}));
    }
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
