//! User-controlled Web Model entrance and conversation binding. The daemon's
//! global write permit serializes these changes with Actor lifecycle changes.
use super::operation::{Operation, Policy::GlobalWrite};
use crate::dispatch::{OpError, OpResult, object, required_arg, string_arg};
use cccc_contracts::{Actor, DaemonRequest, GroupState, RunnerKind};
use cccc_core::{
    GroupDoc, GroupStore, HomeLayout, integration_state, web_model_connectors as store,
};
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
            | "web_model_binding_remove_preserve_unresolved"
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
    if request.op == "web_model_binding_remove_preserve_unresolved" {
        return preserve_unresolved_remove(home, request, &groups, &group, actor, &connector, &id);
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
                | "binding_revision_changed"
        ) {
            return Err(OpError::new("invalid_argument", "Invalid pairing failure"));
        }
        store::fail_pairing(home, &id, &group_id, &actor_id, &pair, &reason)
            .map_err(OpError::io)?;
        return object(json!({"state":"failed","error_code":reason}));
    }
    let automatic = if request.op == "web_model_pairing_confirm" {
        store::pairing_for_actor(&connector, &group_id, &actor_id)
            .is_some_and(|p| p["automatic"] == true)
    } else {
        request.op == "web_model_pairing_begin"
            && request.args.get("automatic") == Some(&json!(true))
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
        let replace = request.args.get("by") == Some(&json!("user"))
            && request.args.get("replace") == Some(&json!(true));
        if !replace
            && store::binding_for_actor(
                &connector,
                &group_id,
                &actor_id,
                &cccc_core::actors::generation_identity(actor),
            )
            .is_some_and(|b| {
                request.op != "web_model_pairing_confirm"
                    || request.args.get("pairing_id") != Some(&b["pairing_id"])
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
    let status = targets[&actor_id]["last_delivery_status"]
        .as_str()
        .unwrap_or("");
    let unresolved = runtime["active_turn_id"]
        .as_str()
        .is_some_and(|s| !s.is_empty())
        || runtime["status"] == "working"
        || matches!(
            status,
            "submitting"
                | "deferred"
                | "pending_new_chat_bind"
                | "legacy_recovery_submitting"
                | "submission_ambiguous"
                | "submission_ambiguous_completion_pending"
                | "completion_ambiguous"
                | "ambiguous"
                | "completion_conflict"
                | "legacy_submission_unverified"
        );
    let normalize_url = store::conversation_url;
    let old_url = targets[&actor_id]["url"]
        .as_str()
        .and_then(|u| normalize_url(u).ok());
    let same_url = string_arg(request, "url")
        .and_then(|u| normalize_url(&u).ok())
        .is_some_and(|u| Some(u) == old_url);
    // Re-pairing the SAME saved conversation restores access without deleting
    // unresolved evidence. A different conversation still requires resolution.
    if unresolved
        && (automatic
            || !(request.op == "web_model_pairing_begin" && old_url.is_some()
                || request.op == "web_model_pairing_confirm" && same_url))
    {
        return Err(OpError::new(
            "delivery_unresolved",
            if old_url.is_none() {
                "This Actor has an unresolved delivery without a stable conversation URL. Review its old chat, then remove and recreate the stopped Actor to pair a new conversation; Group ledger history is retained."
            } else {
                "Re-pair the same saved conversation and review its pending delivery before changing to another conversation"
            },
        ));
    }
    let generation = cccc_core::actors::generation_identity(actor);
    match request.op.as_str() {
        "web_model_pairing_begin" => object(
            store::begin_pairing(home, &id, &group_id, &actor_id, &generation, automatic)
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

const RESOLUTIONS_KEY: &str = "web_model_delivery_resolutions";
const TARGETS_KEY: &str = "web_model_browser_targets";
const PRESERVE_REASONS: [&str; 3] = [
    "bound_conversation_unavailable",
    "owner_abandons_unresolved",
    "manual_replacement_after_review",
];
// cwf#855: only these unresolved statuses may be terminalized without replay.
// `submitting`, `deferred`, `pending_new_chat_bind`, `legacy_recovery_submitting`
// and `completion_conflict` can still represent live work and stay refused.
const PRESERVE_DELIVERY_STATUSES: [&str; 5] = [
    "submission_ambiguous",
    "submission_ambiguous_completion_pending",
    "completion_ambiguous",
    "ambiguous",
    "legacy_submission_unverified",
];
const PRESERVE_ACTIVE_STATUSES: [&str; 5] = [
    "submitting",
    "deferred",
    "pending_new_chat_bind",
    "legacy_recovery_submitting",
    "completion_conflict",
];

/// cwf#855: explicit preserve-and-abandon remove. Normal `web_model_binding_remove`
/// keeps its strict refusal; this path terminalizes exactly one reviewed
/// unresolved delivery as `abandoned_unresolved` — evidence retained, never
/// replayed — and then retires the binding. The audit record is written first
/// and every later step is idempotent under `idempotency_key`, so a lost
/// response or a crash resumes the same transaction instead of duplicating it.
fn preserve_unresolved_remove(
    home: &HomeLayout,
    request: &DaemonRequest,
    groups: &GroupStore,
    group: &GroupDoc,
    actor: &Actor,
    connector: &Value,
    connector_id: &str,
) -> OpResult {
    let reason = required_arg(request, "reason")?;
    let rationale = required_arg(request, "rationale")?;
    let idempotency_key = required_arg(request, "idempotency_key")?;
    let binding_revision = required_arg(request, "binding_revision")?;
    let pairing_id = required_arg(request, "pairing_id")?;
    let delivery_id = required_arg(request, "expected_delivery_id")?;
    if request.args.get("acknowledge_unresolved") != Some(&json!(true))
        || !PRESERVE_REASONS.contains(&reason.as_str())
        || rationale.trim().is_empty()
        || rationale.len() > 1024
        || idempotency_key.len() > 128
        || binding_revision.is_empty()
        || pairing_id.is_empty()
        || delivery_id.is_empty()
    {
        return Err(OpError::new(
            "preserve_reason_required",
            "Preserve-remove needs a typed reason, a nonempty operator rationale, the unresolved acknowledgement and the exact binding/pairing/delivery identity under review",
        ));
    }
    let group_id = group.group_id.as_str();
    let actor_id = actor.id.as_str();
    // A committed resolution returns its original receipt even if the Actor
    // has since been re-enabled or re-paired; a pending one resumes below.
    let prior = find_resolution(groups, group_id, actor_id, &idempotency_key)?;
    if let Some(record) = &prior {
        if !resolution_identity_matches(
            record,
            connector_id,
            &binding_revision,
            &pairing_id,
            &delivery_id,
            &reason,
        ) {
            return Err(OpError::new(
                "resolution_conflict",
                "This idempotency key already belongs to a different removal",
            ));
        }
        if record["state"] == "committed" {
            return object(record["receipt"].clone());
        }
    }
    if actor.enabled {
        return Err(OpError::new(
            "actor_must_be_stopped",
            "Stop this Actor before removing its pairing",
        ));
    }
    if connector["provider"] != "chatgpt_web" {
        return Err(OpError::new(
            "invalid_actor_runtime",
            "Preserve-remove applies to ChatGPT web-model Actors",
        ));
    }
    let generation = cccc_core::actors::generation_identity(actor);
    // The Web worker holds this file lock for its whole browser delivery. A
    // busy lock means browser work is in flight; the transaction refuses
    // rather than racing a submit after the Web-side guard was taken.
    let _browser_work =
        super::actor_runtime::hold_browser_work(home, group_id, actor_id).map_err(|error| {
            if error.code == "actor_browser_busy" {
                OpError::new(
                    "delivery_active",
                    "Browser delivery is in flight; retry after it completes",
                )
            } else {
                error
            }
        })?;
    // Runtime must prove no turn is active; unreadable state fails closed.
    let runtime = super::runtime_state::actor_state(home, group_id, actor_id).map_err(|_| {
        OpError::new(
            "runtime_state_unknown",
            "Cannot prove the Actor has no active turn",
        )
    })?;
    if runtime.is_object() {
        if runtime["active_turn_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
            || runtime["status"] == "working"
            || runtime["active_event_ids"]
                .as_array()
                .is_some_and(|ids| !ids.is_empty())
        {
            return Err(OpError::new(
                "delivery_active",
                "The Actor has an active turn or in-flight delivery",
            ));
        }
        if runtime
            .get("status")
            .is_some_and(|status| !status.is_string())
        {
            return Err(OpError::new(
                "runtime_state_unknown",
                "Actor runtime state is unreadable",
            ));
        }
    }
    // Binding CAS against the exact generation, revision and pairing reviewed.
    // A missing binding is only acceptable when resuming a pending record —
    // an earlier attempt already retired it.
    let binding = store::binding_for_actor(connector, group_id, actor_id, &generation);
    let needs_retire = match (&binding, &prior) {
        (Some(current), _) => {
            if current["revision"].as_str() != Some(binding_revision.as_str())
                || current["pairing_id"].as_str() != Some(pairing_id.as_str())
            {
                return Err(OpError::new(
                    "binding_revision_changed",
                    "The binding changed since it was reviewed",
                ));
            }
            true
        }
        (None, Some(_)) => false,
        (None, None) => {
            return Err(OpError::new(
                "conversation_not_paired",
                "Actor has no current binding",
            ));
        }
    };
    // Delivery CAS: only the exact reviewed unresolved delivery may
    // terminalize. The projection update below re-checks inside its own
    // mutation so a delivery claimed after this read still refuses.
    let targets =
        integration_state::group_get(groups, group_id, TARGETS_KEY).map_err(OpError::io)?;
    let target = targets.get(actor_id).cloned().unwrap_or(Value::Null);
    let previous_status = target["last_delivery_status"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    let already_applied = prior.as_ref().is_some_and(|record| {
        previous_status == "abandoned_unresolved"
            && target["last_delivery_resolution_id"] == record["resolution_id"]
    });
    if target["last_delivery_id"].as_str() != Some(delivery_id.as_str()) {
        return Err(OpError::new(
            "delivery_changed",
            "The pending delivery changed; refresh and review it again",
        ));
    }
    if !already_applied && !PRESERVE_DELIVERY_STATUSES.contains(&previous_status.as_str()) {
        return Err(
            if PRESERVE_ACTIVE_STATUSES.contains(&previous_status.as_str()) {
                OpError::new(
                    "delivery_active",
                    "The delivery is still active work and cannot be abandoned",
                )
            } else {
                OpError::new(
                    "delivery_changed",
                    "The delivery is no longer in an unresolved state",
                )
            },
        );
    }
    // Durable audit record first: pending until every step commits, after
    // which the stored receipt makes retries return the original result.
    let record = match &prior {
        Some(record) => record.clone(),
        None => {
            let record = json!({
                "resolution_id": format!("wmres_{}", uuid::Uuid::new_v4().simple()),
                "idempotency_key": idempotency_key,
                "group_id": group_id,
                "actor_id": actor_id,
                "connector_id": connector_id,
                "actor_generation": generation,
                "binding_revision": binding_revision,
                "pairing_id": pairing_id,
                "delivery_id": delivery_id,
                "turn_id": target["last_delivery_turn_id"],
                "previous_status": previous_status,
                "source_event_ids": target["last_delivery_event_ids"],
                "reason": reason,
                "rationale": rationale,
                "operator": "user",
                "acknowledged_unresolved": true,
                "outcome": "unresolved_preserved_no_replay",
                "state": "pending",
                "created_at": cccc_contracts::utc_now(),
            });
            insert_resolution(groups, group_id, &record)?;
            record
        }
    };
    let resolution_id = record["resolution_id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    // Retire before releasing the fence: if retirement fails the unresolved
    // status still blocks pairing changes and new deliveries.
    let mut retired = Vec::new();
    if needs_retire {
        retired = store::retire_actor(home, group_id, actor_id).map_err(OpError::io)?;
    }
    if !already_applied {
        let applied = integration_state::group_update(groups, group_id, TARGETS_KEY, |value| {
            let targets = value.as_object_mut().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid browser target store",
                )
            })?;
            let entry = targets
                .entry(actor_id.to_owned())
                .or_insert_with(|| json!({}));
            let status = entry["last_delivery_status"].as_str().unwrap_or("");
            if status == "abandoned_unresolved"
                && entry["last_delivery_resolution_id"].as_str() == Some(resolution_id.as_str())
            {
                return Ok(());
            }
            if entry["last_delivery_id"].as_str() != Some(delivery_id.as_str())
                || !PRESERVE_DELIVERY_STATUSES.contains(&status)
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "delivery_changed",
                ));
            }
            entry["last_delivery_status"] = json!("abandoned_unresolved");
            entry["last_delivery_reviewed_at"] = json!(cccc_contracts::utc_now());
            entry["last_delivery_resolution_id"] = json!(resolution_id.as_str());
            entry["last_error"] = json!(
                "Delivery abandoned unresolved by owner decision; evidence preserved and never replayed"
            );
            Ok(())
        });
        if let Err(error) = applied {
            // The fence stays fully in force: roll the connector change back
            // so the unresolved status still guards the still-bound Actor.
            if !retired.is_empty() {
                let _ = store::restore(home, &retired);
            }
            return Err(if error.to_string() == "delivery_changed" {
                OpError::new(
                    "delivery_changed",
                    "The pending delivery changed during removal; the binding was kept",
                )
            } else {
                OpError::io(error)
            });
        }
    }
    // Commit the audit record and answer the receipt. A failure here leaves
    // the pending record for an idempotent retry; the work itself is done.
    let receipt = json!({
        "removed": true,
        "resolution_id": resolution_id,
        "delivery_id": delivery_id,
        "prior_status": previous_status,
        "status": "abandoned_unresolved",
        "binding_revision": binding_revision,
        "replay_performed": false,
    });
    commit_resolution(groups, group_id, &resolution_id, &receipt)?;
    object(receipt)
}

fn find_resolution(
    groups: &GroupStore,
    group_id: &str,
    actor_id: &str,
    idempotency_key: &str,
) -> Result<Option<Value>, OpError> {
    let resolutions =
        integration_state::group_get(groups, group_id, RESOLUTIONS_KEY).map_err(OpError::io)?;
    Ok(resolutions
        .as_object()
        .into_iter()
        .flatten()
        .map(|(_, record)| record)
        .find(|record| {
            let record = *record;
            record["actor_id"].as_str() == Some(actor_id)
                && record["idempotency_key"].as_str() == Some(idempotency_key)
        })
        .cloned())
}

fn resolution_identity_matches(
    record: &Value,
    connector_id: &str,
    binding_revision: &str,
    pairing_id: &str,
    delivery_id: &str,
    reason: &str,
) -> bool {
    record["connector_id"].as_str() == Some(connector_id)
        && record["binding_revision"].as_str() == Some(binding_revision)
        && record["pairing_id"].as_str() == Some(pairing_id)
        && record["delivery_id"].as_str() == Some(delivery_id)
        && record["reason"].as_str() == Some(reason)
}

fn insert_resolution(groups: &GroupStore, group_id: &str, record: &Value) -> Result<(), OpError> {
    let resolution_id = record["resolution_id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    integration_state::group_update(groups, group_id, RESOLUTIONS_KEY, |value| {
        if !value.is_object() {
            *value = json!({});
        }
        let resolutions = value.as_object_mut().expect("resolutions initialized");
        // The GlobalWrite permit serializes daemon writers and the Web never
        // writes this key; the check only guards a duplicated dispatch.
        if resolutions.values().any(|existing| {
            existing["actor_id"] == record["actor_id"]
                && existing["idempotency_key"] == record["idempotency_key"]
        }) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "resolution_exists",
            ));
        }
        resolutions.insert(resolution_id.clone(), record.clone());
        Ok(())
    })
    .map_err(|error| {
        if error.to_string() == "resolution_exists" {
            OpError::new(
                "resolution_conflict",
                "A resolution for this request is already recorded",
            )
        } else {
            OpError::io(error)
        }
    })
}

fn commit_resolution(
    groups: &GroupStore,
    group_id: &str,
    resolution_id: &str,
    receipt: &Value,
) -> Result<(), OpError> {
    integration_state::group_update(groups, group_id, RESOLUTIONS_KEY, |value| {
        let resolutions = value.as_object_mut().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid resolution store")
        })?;
        let record = resolutions.get_mut(resolution_id).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "resolution record missing")
        })?;
        record["state"] = json!("committed");
        record["committed_at"] = json!(cccc_contracts::utc_now());
        record["receipt"] = receipt.clone();
        Ok(())
    })
    .map_err(OpError::io)
}

#[cfg(test)]
mod tests;
