//! User-controlled Web Model entrance and conversation binding. The daemon's
//! global write permit serializes these changes with Actor lifecycle changes.
use super::operation::{Operation, Policy::GlobalWrite};
use crate::dispatch::{OpError, OpResult, object, required_arg, string_arg};
use cccc_contracts::{DaemonRequest, GroupState, RunnerKind};
use cccc_core::{GroupStore, HomeLayout, integration_state, web_model_connectors as store};
use serde_json::{Value, json};

pub(super) fn resolve_operation(request: &DaemonRequest) -> Option<Operation> {
    matches!(
        request.op.as_str(),
        "web_model_connector_configure"
            | "web_model_connector_revoke"
            | "web_model_pairing_begin"
            | "web_model_pairing_accept"
            | "web_model_pairing_confirm"
            | "web_model_pairing_cancel"
            | "web_model_pairing_fail"
            | "web_model_binding_remove"
            | "web_model_grok_bind"
    )
    .then(|| Operation::new(GlobalWrite, execute))
}

fn execute(home: &HomeLayout, request: &DaemonRequest) -> OpResult {
    // These operations are not exposed by the Actor MCP catalog. The private
    // Web port authenticates the entrance before forwarding accept requests.
    if string_arg(request, "by").as_deref() != Some("user") {
        return Err(OpError::new(
            "permission_denied",
            "Web Model pairing is user-controlled",
        ));
    }
    if request.op == "web_model_connector_configure" {
        let provider = match request.args.get("provider") {
            None => "chatgpt_web",
            Some(value) => value
                .as_str()
                .ok_or_else(|| OpError::new("invalid_argument", "provider must be a string"))?,
        };
        let flag = |name: &str| -> Result<bool, OpError> {
            match request.args.get(name) {
                None => Ok(false),
                Some(serde_json::Value::Bool(value)) => Ok(*value),
                Some(_) => Err(OpError::new(
                    "invalid_argument",
                    format!("{name} must be a boolean"),
                )),
            }
        };
        let rotate = flag("rotate")?;
        // Accepted for compatibility with older Web clients; it never changes
        // the safe default. A mistyped value is still refused.
        let create_only = flag("create_only")?;
        if rotate && create_only {
            return Err(OpError::new(
                "invalid_argument",
                "rotate and create_only conflict",
            ));
        }
        if rotate {
            let confirmation = required_arg(request, "confirmation")?;
            // execute has already authorized this daemon caller as user.
            // No request attribution field can override that principal.
            let rotated_by = format!(
                "local:{}",
                string_arg(request, "by").expect("authorized caller")
            );
            return object(
                store::rotate_provider(home, provider, &confirmation, &rotated_by).map_err(
                    |e| match e.to_string().as_str() {
                        "rotation_confirmation_stale" => OpError::new(
                            "rotation_confirmation_stale",
                            "The connector changed since the rotation was previewed; refresh and confirm again",
                        ),
                        "connector_unavailable" => OpError::new(
                            "connector_unavailable",
                            "No active connector to rotate",
                        ),
                        _ => OpError::io(e),
                    },
                )?,
            );
        }
        return match store::create_provider_if_absent(home, provider).map_err(OpError::io)? {
            Some(created) => object(created),
            None => {
                // One idempotent success for every non-rotating call: the
                // existing connector and no credential.
                let connector = store::load(home)
                    .map_err(OpError::io)?
                    .into_iter()
                    .find(|c| c["provider"] == provider && c["revoked"] != true)
                    .ok_or_else(|| OpError::new("connector_unavailable", "connector vanished"))?;
                object(json!({"outcome":"existing","connector":connector}))
            }
        };
    }
    let id = required_arg(request, "connector_id")?;
    if request.op == "web_model_connector_revoke" {
        return object(json!({"revoked":store::revoke(home, &id).map_err(OpError::io)?}));
    }
    if request.op == "web_model_pairing_accept" {
        let code = required_arg(request, "code")?;
        let session = required_arg(request, "session_key")?;
        // Browser candidates remain pending until receipt verification. Dots
        // commits only its explicitly selected route after lifecycle validation.
        let pair = store::accept_pairing(home, &id, &code, &session).map_err(OpError::io)?;
        if pair["transport"] == "mcp_events" {
            return super::dots_events::accept_pairing(home, &id, &pair);
        }
        return object(
            json!({"state":pair["state"],"pairing_id":pair["pairing_id"],"receipt":pair["receipt"]}),
        );
    }
    let group_id = required_arg(request, "group_id")?;
    let actor_id = required_arg(request, "actor_id")?;
    let groups = GroupStore::new(home.clone()).map_err(OpError::io)?;
    let group = groups.load(&group_id).map_err(OpError::not_found)?;
    let actor = group
        .actors
        .iter()
        .find(|a| a.id == actor_id)
        .ok_or_else(|| OpError::new("actor_not_found", "Actor not found"))?;
    // The switch retires the old routes when it completes, and a route made
    // before that would go with them.
    super::actor_switch::refuse_pending(home, &group_id, &actor_id)?;
    if !actor.runtime.is_web_model() || actor.runner != RunnerKind::Headless {
        return Err(OpError::new(
            "invalid_actor_runtime",
            "Pairing requires a Web Model Actor",
        ));
    }
    if actor.runtime == cccc_contracts::ActorRuntime::ChatgptDots {
        return Err(OpError::new(
            "dots_requires_events",
            "Dots connects through MCP Events, not conversation pairing",
        ));
    }
    let connector = store::load(home)
        .map_err(OpError::io)?
        .into_iter()
        .find(|c| c["connector_id"] == id && c["revoked"] != true)
        .ok_or_else(|| {
            OpError::new(
                "connector_unavailable",
                "Configure the shared connector first",
            )
        })?;
    if actor.runtime.web_model_provider() != connector["provider"].as_str()
        || (request.op.starts_with("web_model_pairing_") && connector["provider"] != "chatgpt_web")
        || (request.op == "web_model_grok_bind" && connector["provider"] != "grok_web")
    {
        return Err(OpError::new(
            "invalid_actor_runtime",
            "Connector provider does not match this Actor",
        ));
    }
    if connector["provider"] == "grok_web" {
        if actor.enabled && request.op != "web_model_grok_bind" {
            return Err(OpError::new(
                "actor_must_be_stopped",
                "Stop CCCC access before changing the Grok connection",
            ));
        }
        if super::runtime_state::actor_state(home, &group_id, &actor_id)?["active_turn_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
        {
            return Err(OpError::new(
                "delivery_unresolved",
                "Resolve the unfinished turn before changing the Grok connection",
            ));
        }
        return if request.op == "web_model_grok_bind" {
            let binding = store::bind_grok(
                home,
                &id,
                &group_id,
                &actor_id,
                &cccc_core::actors::generation_identity(actor),
                &required_arg(request, "url")?,
            )
            .map_err(OpError::io)?;
            object(json!({"state":"bound","url":binding["url"]}))
        } else if request.op == "web_model_binding_remove" {
            store::retire_actor(home, &group_id, &actor_id).map_err(OpError::io)?;
            object(json!({"removed":true}))
        } else {
            Err(OpError::new(
                "invalid_operation",
                "Grok uses native Webhook setup",
            ))
        };
    }
    if request.op == "web_model_pairing_cancel" {
        let pairing_id = required_arg(request, "pairing_id")?;
        store::cancel_pairing(home, &id, &group_id, &actor_id, &pairing_id).map_err(OpError::io)?;
        return object(json!({"cancelled":true}));
    }
    if request.op == "web_model_pairing_fail" {
        let pair = required_arg(request, "pairing_id")?;
        let reason = required_arg(request, "error_code")?;
        if !matches!(
            reason.as_str(),
            "pairing_interrupted"
                | "pairing_timeout"
                | "pairing_target_changed"
                | "pairing_submission_uncertain"
                | "pairing_composer_occupied"
                | "pairing_failed"
        ) {
            return Err(OpError::new("invalid_argument", "Invalid pairing failure"));
        }
        store::fail_pairing(home, &id, &group_id, &actor_id, &pair, &reason)
            .map_err(OpError::io)?;
        return object(json!({"state":"failed","error_code":reason}));
    }
    let generation = cccc_core::actors::generation_identity(actor);
    let binding = store::binding_for_actor(&connector, &group_id, &actor_id, &generation);
    let pairing = store::pairing_for_actor(&connector, &group_id, &actor_id);
    let confirming = request.op == "web_model_pairing_confirm";
    let beginning = request.op == "web_model_pairing_begin";
    let replace = request.args.get("replace") == Some(&json!(true));
    let automatic = if confirming {
        pairing.as_ref().is_some_and(|p| p["automatic"] == true)
    } else {
        beginning && request.args.get("automatic") == Some(&json!(true))
    };
    if automatic {
        if !actor.enabled
            || !group.running
            || matches!(group.state, GroupState::Paused | GroupState::Stopped)
        {
            return Err(OpError::new(
                "actor_not_active",
                "Automatic connection requires an enabled Actor in a running Group",
            ));
        }
        // cwf#821: an explicit user re-pair (`replace: true`) may supersede the
        // existing binding. It is never removed up front: the old binding stays
        // in force until this pairing's confirm overwrites it, so a failed or
        // abandoned attempt leaves the working route untouched.
        if !replace
            && binding.as_ref().is_some_and(|b| {
                !confirming || request.args.get("pairing_id") != Some(&b["pairing_id"])
            })
        {
            return Err(OpError::new(
                "already_paired",
                "Automatic connection cannot replace an existing binding",
            ));
        }
    } else if actor.enabled {
        return Err(OpError::new(
            "actor_must_be_stopped",
            "Stop this Actor before changing its conversation pairing",
        ));
    }
    let runtime = super::runtime_state::actor_state(home, &group_id, &actor_id)?;
    let targets = integration_state::group_get(&groups, &group_id, "web_model_browser_targets")
        .map_err(OpError::io)?;
    let target = &targets[&actor_id];
    let active = runtime["active_turn_id"]
        .as_str()
        .is_some_and(|s| !s.is_empty())
        || runtime["status"] == "working";
    let unresolved = active
        || store::delivery_fenced(target)
            && !binding
                .as_ref()
                .is_some_and(|b| store::abandoned_delivery_matches(b, target));
    let old_url = target["url"]
        .as_str()
        .filter(|u| store::conversation_identity(u).is_some());
    let same_url = string_arg(request, "url")
        .is_some_and(|url| old_url.is_some_and(|old| store::same_conversation(old, &url)));
    let already_confirmed = confirming
        && binding
            .as_ref()
            .is_some_and(|b| request.args.get("pairing_id") == Some(&b["pairing_id"]));
    let requested_recovery = if beginning
        && (request.args.contains_key("abandon_delivery_id")
            || request.args.contains_key("expected_binding_revision"))
    {
        Some(json!({
            "delivery_id": required_arg(request, "abandon_delivery_id")?,
            "binding_revision": required_arg(request, "expected_binding_revision")?,
        }))
    } else if confirming && !already_confirmed {
        pairing
            .as_ref()
            .filter(|p| request.args.get("pairing_id") == Some(&p["pairing_id"]))
            .and_then(|p| p.get("recovery"))
            .cloned()
    } else {
        None
    };
    let recovery = requested_recovery
        .as_ref()
        .map(|expected| {
            if !replace || active || same_url {
                return Err(repair_refused());
            }
            let current = recovery_evidence(
                home,
                &group_id,
                &actor_id,
                binding.as_ref(),
                target,
                expected,
            )?;
            if confirming && &current != expected {
                return Err(repair_refused());
            }
            Ok(current)
        })
        .transpose()?;
    // Re-pairing the SAME saved conversation restores access without deleting
    // unresolved evidence. A different conversation still requires resolution.
    let stopped_same_chat =
        !automatic && (beginning && old_url.is_some() || confirming && same_url);
    let explicit_same_chat = replace && !active && same_url && (beginning || confirming);
    if unresolved && !(stopped_same_chat || explicit_same_chat || recovery.is_some()) {
        return Err(OpError::new(
            "delivery_unresolved",
            if old_url.is_none() {
                "This Actor has an unresolved delivery without a stable conversation URL. Review its old chat, then remove and recreate the stopped Actor to pair a new conversation; Group ledger history is retained."
            } else {
                "Re-pair the same saved conversation and review its pending delivery before changing to another conversation"
            },
        ));
    }
    match request.op.as_str() {
        "web_model_pairing_begin" => object(
            store::begin_pairing_with_recovery(
                home,
                &id,
                &group_id,
                &actor_id,
                &generation,
                automatic,
                recovery,
            )
            .map_err(OpError::io)?,
        ),
        "web_model_pairing_confirm" => {
            let pair = required_arg(request, "pairing_id")?;
            let url = required_arg(request, "url")?;
            let binding =
                store::confirm_pairing(home, &id, &group_id, &actor_id, &generation, &pair, &url)
                    .map_err(OpError::io)?;
            object(json!({"state":"bound","url":binding["url"],"revision":binding["revision"]}))
        }
        "web_model_binding_remove" => {
            store::retire_actor(home, &group_id, &actor_id).map_err(OpError::io)?;
            object(json!({"removed":true}))
        }
        _ => unreachable!(),
    }
}

fn repair_refused() -> OpError {
    OpError::new(
        "delivery_unresolved",
        "Replacement requires the current binding and a completed ambiguous delivery. Refresh status before retrying.",
    )
}

fn recovery_evidence(
    home: &HomeLayout,
    group: &str,
    actor: &str,
    binding: Option<&Value>,
    target: &Value,
    expected: &Value,
) -> Result<Value, OpError> {
    let binding = binding.ok_or_else(repair_refused)?;
    let revision = expected["binding_revision"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(repair_refused)?;
    let delivery = expected["delivery_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(repair_refused)?;
    if binding["revision"] != revision
        || target["binding_revision"] != revision
        || target["kind"] != "existing_chat"
        || target["last_delivery_status"] != "submission_ambiguous"
        || target["last_delivery_id"] != delivery
    {
        return Err(repair_refused());
    }
    let turn = target["last_delivery_turn_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(repair_refused)?;
    let events = serde_json::from_value::<Vec<String>>(target["last_delivery_event_ids"].clone())
        .ok()
        .filter(|events| !events.is_empty())
        .ok_or_else(repair_refused)?;
    if !super::runtime_completion::committed(home, group, actor, turn, &events)?
        .is_some_and(|receipt| receipt.status == "done" && receipt.delivery_id == delivery)
    {
        return Err(repair_refused());
    }
    Ok(
        json!({"binding_revision":revision,"delivery_id":delivery,"turn_id":turn,"event_ids":events}),
    )
}

#[cfg(test)]
mod tests;
