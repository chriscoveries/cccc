use cccc_core::{GroupStore, integration_state, web_model_connectors};
use serde_json::{Value, json};

use crate::AppState;
use crate::api::ApiError;

use super::web_model_browser::TARGETS_KEY;
use super::web_model_connector_activity::{self as activity, Activity};
use super::web_model_connector_store;

pub(super) fn target(state: &AppState, group_id: &str, actor_id: &str) -> Result<Value, ApiError> {
    let store = GroupStore::new(state.home.clone()).map_err(io_error)?;
    let mut target = integration_state::group_get(&store, group_id, TARGETS_KEY)
        .map_err(io_error)?
        .get(actor_id)
        .cloned()
        .unwrap_or_else(|| json!({}));
    let Some(binding) = web_model_connector_store::for_actor(state, group_id, actor_id) else {
        // Preserve historical/ambiguous evidence, but never authorize an old
        // saved URL without a current conversation binding.
        target["kind"] = json!("none");
        target["state"] = json!("pairing_required");
        target["next_delivery"] = json!("blocked");
        return Ok(target);
    };
    bound_target(target, &binding)
}

fn bound_target(mut target: Value, binding: &Value) -> Result<Value, ApiError> {
    let same_chat = target["url"]
        .as_str()
        .zip(binding["url"].as_str())
        .is_some_and(|(old, new)| web_model_connectors::same_conversation(old, new));
    let abandoned = web_model_connectors::abandoned_delivery_matches(binding, &target);
    // The exact acknowledgement survives later replacements, including a
    // return to this chat before the new binding wrote its first target row.
    if abandoned
        || (target["binding_revision"] != binding["revision"]
            && target["url"] != binding["url"]
            && !same_chat)
    {
        if binding["abandoned_delivery"].is_object()
            && web_model_connectors::delivery_fenced(&target)
            && !abandoned
        {
            return Err(ApiError::conflict(
                "delivery_unresolved",
                "The pending delivery changed during replacement. Review it before resuming.",
                json!({}),
            ));
        }
        target = json!({});
    }
    target["binding_revision"] = binding["revision"].clone();
    target["url"] = binding["url"].clone();
    target["kind"] = json!("existing_chat");
    target["state"] = json!("bound_existing_chat");
    target["saved_at"] = binding["updated_at"].clone();
    target["next_delivery"] = json!("existing_chat");
    Ok(target)
}

pub(super) fn update_target(
    state: &AppState,
    group_id: &str,
    actor_id: &str,
    patch: Value,
) -> Result<(), ApiError> {
    let store = GroupStore::new(state.home.clone()).map_err(io_error)?;
    let effective = target(state, group_id, actor_id)?;
    integration_state::group_update(&store, group_id, TARGETS_KEY, |value| {
        let targets = value.as_object_mut().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid browser target store",
            )
        })?;
        let target = targets.entry(actor_id).or_insert_with(|| json!({}));
        if effective["binding_revision"].is_string()
            && target["binding_revision"] != effective["binding_revision"]
        {
            *target = effective.clone();
        }
        let target = target.as_object_mut().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid browser target")
        })?;
        target.extend(patch.as_object().cloned().unwrap_or_default());
        // cwf#821: keep the last confirmed submission time apart from
        // last_delivery_at, which also moves on claims and failed attempts.
        if matches!(
            patch["last_delivery_status"].as_str(),
            Some("submitted" | "bound")
        ) && patch["last_submitted_at"].is_null()
        {
            target.insert("last_submitted_at".into(), json!(cccc_contracts::utc_now()));
        }
        Ok(())
    })
    .map_err(io_error)
}

pub(super) fn record_connector(
    state: &AppState,
    group_id: &str,
    actor_id: &str,
    status: &str,
    turn_id: &str,
    error: &str,
) -> Result<(), ApiError> {
    let Some(connector) = web_model_connector_store::for_actor(state, group_id, actor_id) else {
        return Ok(());
    };
    activity::record_route(
        state,
        connector["connector_id"].as_str().unwrap_or(""),
        Some(&connector),
        Activity {
            method: "browser/delivery",
            tool_name: "",
            call_status: status,
            wait_status: status,
            turn_id,
            error,
        },
        None,
    )
}

fn io_error(error: std::io::Error) -> ApiError {
    ApiError::bad(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_return_to_the_original_chat_keeps_the_abandoned_delivery_retired() {
        let original = json!({
            "kind":"existing_chat","url":"https://chatgpt.com/c/original","binding_revision":"first",
            "last_delivery_status":"submission_ambiguous","last_delivery_id":"old-delivery",
            "last_delivery_turn_id":"old-turn","last_delivery_event_ids":["old-event"]
        });
        let mut binding = json!({
            "url":"https://chatgpt.com/c/replacement","revision":"second",
            "abandoned_delivery":{"binding_revision":"first","delivery_id":"old-delivery",
                "turn_id":"old-turn","event_ids":["old-event"]}
        });
        let changed = bound_target(original.clone(), &binding).expect("replacement projection");
        assert!(changed["last_delivery_status"].is_null());

        // No delivery writes the replacement target before the chat is rebound.
        binding["revision"] = json!("third");
        for route in [
            "https://chatgpt.com/c/original",
            "https://chatgpt.com/g/g-p-fixture/c/original",
        ] {
            binding["url"] = json!(route);
            let restored = bound_target(original.clone(), &binding).expect("returned route");
            assert_eq!(restored["url"], route);
            assert_eq!(restored["binding_revision"], "third");
            assert!(restored["last_delivery_id"].is_null());
            assert!(restored["last_delivery_status"].is_null());
        }

        let mut newer = original;
        newer["last_delivery_id"] = json!("newer-delivery");
        assert_eq!(
            bound_target(newer.clone(), &binding).expect("same chat retains newer evidence")["last_delivery_id"],
            "newer-delivery"
        );
        binding["url"] = json!("https://chatgpt.com/c/new");
        assert_eq!(
            bound_target(newer, &binding)
                .expect_err("newer fence blocks a different chat")
                .code(),
            "delivery_unresolved"
        );
    }
}
