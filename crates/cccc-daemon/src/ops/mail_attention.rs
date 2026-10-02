use super::operation::{Operation, Policy};
use crate::dispatch::{OpError, OpResult, object, required_arg, store, string_arg};
use cccc_contracts::DaemonRequest;
use cccc_core::HomeLayout;
use serde_json::json;

pub(super) fn resolve_operation(request: &DaemonRequest) -> Option<Operation> {
    Some(match request.op.as_str() {
        "mail_attention_context" => Operation::new(Policy::Write, context),
        "mail_attention_status" => Operation::new(Policy::Read, status),
        _ => return None,
    })
}

fn owner(home: &HomeLayout, request: &DaemonRequest) -> Result<(String, String), OpError> {
    let group_id = required_arg(request, "group_id")?;
    let actor_id = required_arg(request, "actor_id")?;
    if actor_id == "user" || string_arg(request, "by").as_deref() != Some(&actor_id) {
        return Err(OpError::new(
            "permission_denied",
            "mail context belongs to the authenticated calling actor",
        ));
    }
    let group = store(home)?.load(&group_id).map_err(OpError::io)?;
    if cccc_core::actors::find(&group, &actor_id).is_none() {
        return Err(OpError::new(
            "actor_not_found",
            "mail attention actor not found",
        ));
    }
    Ok((group_id, actor_id))
}

fn context(home: &HomeLayout, request: &DaemonRequest) -> OpResult {
    let (group_id, actor_id) = owner(home, request)?;
    let carrier = required_arg(request, "carrier_id")?;
    // Coordination inside a reminder turn must not feed another reminder,
    // even when the actor takes longer than the next backoff interval.
    let active = super::runtime_state::actor_state(home, &group_id, &actor_id)?;
    if active["status"] == "working" {
        let ids = active["active_event_ids"].as_array();
        let path = store(home)?.ledger_path(&group_id).map_err(OpError::io)?;
        let reminder = cccc_core::ledger::inspect(&path, |events, _| {
            events.iter().any(|e| {
                ids.is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(&e.id)))
                    && cccc_core::mail_attention::is_mail_originated(&group_id, e)
            })
        })
        .map_err(OpError::io)?;
        if reminder {
            return object(json!({"mail_pending":null}));
        }
    }
    let hint = cccc_core::mail_attention::offer_context(
        home,
        &group_id,
        &actor_id,
        &carrier,
        chrono::Utc::now().timestamp(),
    )
    .map_err(OpError::io)?;
    object(json!({"mail_pending":hint}))
}

fn status(home: &HomeLayout, request: &DaemonRequest) -> OpResult {
    let (group_id, actor_id) = owner(home, request)?;
    let summary = cccc_core::mail_attention::inspect(
        home,
        &group_id,
        &actor_id,
        chrono::Utc::now().timestamp(),
    )
    .map_err(OpError::io)?;
    object(json!({"mail_attention":summary}))
}
