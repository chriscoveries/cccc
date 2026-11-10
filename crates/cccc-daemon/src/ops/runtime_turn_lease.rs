//! Shared Dots/Hark abandonment policy: release ownership, never replay inputs.
use crate::dispatch::OpError;
use cccc_contracts::Actor;
use cccc_core::{GroupDoc, HomeLayout};
use serde_json::Value;

pub(super) const TURN_LEASE_SECONDS: u64 = 1800;

/// Use the matching execution clock; a canonical state stamp is a conservative
/// fallback when a crash lost the private projection. Unknown clocks fail closed.
pub(super) fn elapsed_seconds(runtime: &Value, execution: &Value) -> Option<u64> {
    let now = chrono::Utc::now();
    super::runtime_state::active_turn_elapsed_seconds(runtime, execution, now).or_else(|| {
        let stamp = if runtime["active_turn_id"].as_str().is_none_or(str::is_empty)
            && matches!(execution["state"].as_str(), Some("active" | "interrupted"))
        {
            &execution["claimed_at"]
        } else {
            &runtime["updated_at"]
        };
        let updated = chrono::DateTime::parse_from_rfc3339(stamp.as_str()?).ok()?;
        now.signed_duration_since(updated)
            .to_std()
            .ok()
            .map(|duration| duration.as_secs())
    })
}

/// Called under the Group write permit and the connector store lock. This must
/// not update the connector: the caller commits its private projection on the
/// snapshot it already owns, avoiding a nested connector lock.
pub(super) fn expire(
    home: &HomeLayout,
    group: &GroupDoc,
    actor: &Actor,
    runtime: &Value,
    execution: &Value,
    reason: &str,
) -> Result<String, OpError> {
    // A crash can clear canonical ownership before saving the private state.
    // Consult that exact stale execution's receipt without touching a new owner.
    let canonical = runtime["active_turn_id"]
        .as_str()
        .filter(|id| !id.is_empty());
    let turn_id = canonical.unwrap_or_else(|| execution["turn_id"].as_str().unwrap_or_default());
    let ids = if canonical.is_some() {
        &runtime["active_event_ids"]
    } else {
        &execution["event_ids"]
    };
    let event_ids: Vec<String> = ids
        .as_array()
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let mut status = "expired".to_owned();
    if !turn_id.is_empty() && !event_ids.is_empty() {
        // A crash can leave a committed receipt without its projections; keep
        // that durable outcome rather than recording an expiry on top of it.
        let completion = match super::runtime_completion::committed(
            home,
            &group.group_id,
            &actor.id,
            turn_id,
            &event_ids,
        )? {
            Some(committed) => {
                status = committed.status.clone();
                committed
            }
            None => super::runtime_completion::Completion {
                turn_id: turn_id.to_owned(),
                event_ids: event_ids.clone(),
                status: status.clone(),
                delivery_id: format!("runtime:{turn_id}"),
            },
        };
        for event_id in &completion.event_ids {
            if let Some((state, transport)) =
                super::runtime_delivery::latest_state(home, &group.group_id, &actor.id, event_id)?
                && state == "claimed"
            {
                super::runtime_delivery::append_state(
                    home,
                    &group.group_id,
                    &actor.id,
                    &actor.created_at,
                    event_id,
                    &transport,
                    super::runtime_delivery::DeliveryOutcome::Ambiguous(reason),
                )?;
            }
        }
        super::runtime_completion::append(home, &group.group_id, &actor.id, &completion)?;
        // This clears canonical ownership only; the private projection below is
        // updated on the same connector snapshot the claim already holds.
        let fresh = super::runtime_state::actor_state(home, &group.group_id, &actor.id)?;
        if fresh["active_turn_id"].as_str() == Some(turn_id) {
            super::runtime_state::set_runtime_status(
                home,
                group,
                &actor.id,
                "waiting",
                "",
                "",
                &[],
            )?;
        }
    }
    Ok(status)
}
