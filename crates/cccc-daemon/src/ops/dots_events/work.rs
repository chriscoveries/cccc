use super::*;

pub(super) fn claim(home: &HomeLayout, request: &DaemonRequest) -> OpResult {
    let (group, actor) = group_actor(home, request)?;
    let captured = request
        .args
        .get("_cccc_web_binding")
        .filter(|b| b.is_object())
        .ok_or_else(|| {
            OpError::new(
                "permission_denied",
                "A current paired Dots session is required",
            )
        })?;
    if string_arg(request, "by").as_deref() != Some(actor.id.as_str()) {
        return Err(OpError::new(
            "permission_denied",
            "Dots work must be claimed by its bound Actor",
        ));
    }
    if captured["dots_stage"] != "claim" {
        return Err(OpError::new(
            "permission_denied",
            "Dots work requires its paired session's claim authority",
        ));
    }
    let id = captured["connector_id"]
        .as_str()
        .ok_or_else(|| OpError::new("permission_denied", "Dots binding is required"))?;
    let captured_wake = captured
        .get("dots_wake_id")
        .filter(|wake| wake.is_null() || wake.as_str().is_some_and(|id| !id.is_empty()))
        .ok_or_else(|| OpError::new("permission_denied", "A Dots wake snapshot is required"))?;
    let mut pulled = request.clone();
    pulled
        .args
        .insert("transport".into(), json!("web_model_pull"));
    let result = store::dots_update(home, id, |c| {
        let Some(route) = binding(c, &group, &actor) else {
            return Err(denied("dots_binding_changed"));
        };
        if route["revision"] != captured["revision"]
            || route["session_key"].as_str().is_none_or(str::is_empty)
            || route["session_key"] != captured["session_key"]
            || c["dots"]["epoch"] != captured["dots_epoch"]
            || c["dots"]["wake"]["event_id"] != *captured_wake
        {
            return Err(denied("dots_task_expired"));
        }
        let runtime = super::super::runtime_state::actor_state(home, &group.group_id, &actor.id)
            .map_err(|_| denied("dots_runtime_state_unavailable"))?;
        // Canonical ownership wins even if a crash left the private execution
        // projection absent or only part of the source admission committed.
        // Expiry precedes duplicate-wake checks. Interrupted projections can
        // coexist with a pending new wake; resolve the old turn without
        // consuming that wake or hiding unfinished canonical ownership.
        let unfinished = runtime["active_turn_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
            || c["dots"]["execution"]["state"] == "active"
            || (!captured_wake.is_null()
                && c["dots"]["execution"]["event_id"] == *captured_wake);
        if unfinished {
            let elapsed = super::super::runtime_state::active_turn_elapsed_seconds(
                &runtime,
                &c["dots"]["execution"],
                chrono::Utc::now(),
            )
            .or_else(|| {
                // A receipt can clear canonical ownership before the private
                // projection commits. Preserve the original claim's lease.
                if runtime["active_turn_id"].as_str().is_none_or(str::is_empty) {
                    age_seconds(c["dots"]["execution"]["claimed_at"].as_str())
                } else {
                    None
                }
            })
            .or_else(|| turn_age_seconds(&runtime));
            if elapsed.is_none_or(|elapsed| elapsed < TURN_LEASE_SECONDS) {
                return Ok(Ok(json!({"status":"turn_in_progress","turn":null,
                    "active_turn_id":runtime["active_turn_id"],
                    "event_ids":runtime["active_event_ids"],
                    "active_turn_elapsed_seconds":elapsed,
                    "reason_code":"dots_turn_unfinished",
                    "instructions":"No new work was claimed. Let the original run finish with its own claim result; do not retrieve credentials from another run or claim again. If that run cannot finish, have the owner review its work in ChatGPT and use End this turn in this Actor's CCCC pane. Ending a turn does not stop cloud work or replay its messages. A later claim can expire an abandoned turn after its lease elapses."})));
            }
            expire_active_turn(home, &group, &actor, &runtime, c)?;
        }
        let dots = &c["dots"];
        // A repeated wake after completion is idle, including a turn just
        // expired above. Canonical/private ownership was checked first.
        if !captured_wake.is_null() && dots["last_execution"]["event_id"] == *captured_wake {
            return Ok(Ok(json!({
                "status":"idle",
                "turn":null,
                "reason_code":"dots_wake_already_completed"
            })));
        }
        let Some(event_id) = captured_wake.as_str() else {
            return Ok(Ok(json!({"status":"idle","turn":null})));
        };
        if !active(&group, &actor)
            || !matches!(
                dots["wake"]["state"].as_str(),
                Some("pending" | "sending" | "delivered")
            )
            || !subscribed(&dots["subscription"], chrono::Utc::now().timestamp_millis())
        {
            return Ok(Ok(json!({"status":"idle","turn":null})));
        }
        let result = super::super::runtime_state::wait_next_turn(home, &pulled);
        match result {
            Err(error) => Ok(Err(error)),
            Ok(result) => {
                let mut result = Value::Object(result);
                if result["status"] == "work_available" {
                    c["dots"]["execution"] = json!({"event_id":event_id,"epoch":c["dots"]["epoch"],
                        "turn_id":result["turn"]["turn_id"],"event_ids":result["turn"]["event_ids"],
                        "state":"active","claimed_at":cccc_contracts::utc_now()});
                    c["dots"]["wake"]["state"] = json!("claimed");
                    // This value is returned only after dots_update commits the
                    // private projection. It never enters the wake or ledger.
                    result["actor_token"] = json!(store::dots_wake_token(c, &route, event_id)?);
                } else if result["status"] == "idle" {
                    c["dots"]["wake"]["state"] = json!("completed");
                }
                Ok(Ok(result))
            }
        }
    })
    .map_err(state_error)??;
    let result = object(result)?;
    if let Some(turn_id) = result
        .get("turn")
        .and_then(|turn| turn.get("turn_id"))
        .and_then(Value::as_str)
    {
        super::super::runtime_state::confirm_pulled_instruction_notice(
            home,
            &group.group_id,
            &actor.id,
            turn_id,
        );
    }
    Ok(result)
}

pub(in crate::ops) fn record_completed(
    home: &HomeLayout,
    group: &GroupDoc,
    actor_id: &str,
    completion: &super::super::runtime_completion::Completion,
    user_resolution: bool,
) -> Result<(), OpError> {
    let actor = group
        .actors
        .iter()
        .find(|a| a.id == actor_id)
        .ok_or_else(|| OpError::new("actor_not_found", "Actor not found"))?;
    let Some(c) = connector(home, group, actor)? else {
        if user_resolution {
            return Ok(());
        }
        return Err(OpError::new(
            "dots_binding_unavailable",
            "The Dots binding is unavailable",
        ));
    };
    let id = c["connector_id"].as_str().ok_or_else(|| {
        OpError::new(
            "dots_binding_unavailable",
            "The Dots binding is unavailable",
        )
    })?;
    store::dots_update(home, id, |c| {
        let dots = &mut c["dots"];
        if dots["last_execution"]["turn_id"] == completion.turn_id
            && dots["last_execution"]["event_ids"] == json!(completion.event_ids)
            && dots["last_execution"]["status"] == completion.status
        {
            return Ok(());
        }
        let execution = &dots["execution"];
        if execution["turn_id"] != completion.turn_id
            || execution["event_ids"] != json!(completion.event_ids)
        {
            return Err(denied("dots_completion_conflict"));
        }
        let mut last = execution.clone();
        last["state"] = json!("completed");
        last["status"] = json!(completion.status);
        last["completed_at"] = json!(cccc_contracts::utc_now());
        // Explicit user resolution uses the current epoch; old remote authority
        // remains retired even when the receipt resolves an interrupted turn.
        last["epoch"] = dots["epoch"].clone();
        dots["last_execution"] = last;
        dots["execution"] = Value::Null;
        dots["wake"]["state"] = json!("completed");
        Ok(())
    })
    .map_err(state_error)
}

pub(super) fn control(home: &HomeLayout, request: &DaemonRequest) -> OpResult {
    user(request)?;
    let (group, actor) = group_actor(home, request)?;
    if required_arg(request, "action")? != "resolve" {
        return Err(OpError::new(
            "invalid_action",
            "Only explicit unfinished-turn resolution is supported",
        ));
    }
    let runtime = super::super::runtime_state::actor_state(home, &group.group_id, &actor.id)?;
    let turn_id = required_arg(request, "turn_id")?;
    let event_ids = request
        .args
        .get("event_ids")
        .and_then(Value::as_array)
        .filter(|ids| !ids.is_empty() && ids.len() <= 20 && ids.iter().all(Value::is_string))
        .ok_or_else(|| {
            OpError::new(
                "invalid_event_ids",
                "The exact unfinished event_ids are required",
            )
        })?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if runtime["active_turn_id"] != turn_id || runtime["active_event_ids"] != json!(event_ids) {
        return Err(OpError::new(
            "stale_turn",
            "This is not the current unfinished Dots turn",
        ));
    }
    let committed = super::super::runtime_completion::committed(
        home,
        &group.group_id,
        &actor.id,
        &turn_id,
        &event_ids,
    )?;
    // This resolves CCCC authority, not the cloud process. No work is replayed.
    if let Some(c) = connector(home, &group, &actor)? {
        invalidate(home, &group, &actor.id)?;
        let id = c["connector_id"].as_str().ok_or_else(|| {
            OpError::new(
                "dots_binding_unavailable",
                "The Dots binding is unavailable",
            )
        })?;
        // A crash between the existing turn commit and the private projection may
        // leave no execution projection. The exact durable turn remains authority
        // for this explicit user resolution; it is never returned to cloud work.
        store::dots_update(home, id, |c| {
        if c["dots"]["execution"].is_null() {
            c["dots"]["execution"] = json!({"event_id":c["dots"]["wake"]["event_id"],
                "epoch":c["dots"]["epoch"],"turn_id":turn_id,"event_ids":event_ids,"state":"interrupted"});
        }
        Ok(())
        }).map_err(state_error)?;
    }
    let completion = committed.unwrap_or_else(|| super::super::runtime_completion::Completion {
        turn_id,
        event_ids,
        status: "cancelled".into(),
        delivery_id: format!(
            "runtime:{}",
            runtime["active_turn_id"].as_str().unwrap_or_default()
        ),
    });
    // The turn projection precedes individual accepted receipts. A crash in
    // that interval must not let explicit resolution requeue its old inputs.
    for event_id in &completion.event_ids {
        if let Some((state, transport)) = super::super::runtime_delivery::latest_state(
            home,
            &group.group_id,
            &actor.id,
            event_id,
        )? && state == "claimed"
        {
            super::super::runtime_delivery::append_state(
                home,
                &group.group_id,
                &actor.id,
                &actor.created_at,
                event_id,
                &transport,
                super::super::runtime_delivery::DeliveryOutcome::Ambiguous(
                    "Dots unfinished turn was explicitly resolved by the user; its inputs must not be replayed",
                ),
            )?;
        }
    }
    let receipt =
        super::super::runtime_completion::append(home, &group.group_id, &actor.id, &completion)?;
    super::super::runtime_state::finish_completion(
        home,
        &group,
        &actor.id,
        &completion,
        receipt,
        request,
    )
}

/// How long a claimed Dots turn may run before a claim expires it. Remote
/// tasks are short; anything older is abandoned work, not a busy actor.
const TURN_LEASE_SECONDS: u64 = 1800;

/// Fallback clock when the private execution projection is absent or does not
/// match the canonical turn: the runtime state stamp still bounds its age.
fn turn_age_seconds(runtime: &Value) -> Option<u64> {
    age_seconds(runtime["updated_at"].as_str())
}

fn age_seconds(stamp: Option<&str>) -> Option<u64> {
    let updated = chrono::DateTime::parse_from_rfc3339(stamp?).ok()?;
    chrono::Utc::now()
        .signed_duration_since(updated)
        .to_std()
        .ok()
        .map(|duration| duration.as_secs())
}

/// Expire an abandoned claimed turn so the actor can claim new work. This is
/// the same durable resolution an explicit user `resolve` produces — a
/// completion receipt, inputs marked ambiguous so nothing replays, canonical
/// ownership cleared — except it triggers on the lease instead of a user
/// action and returns the actor to work in the same claim.
fn expire_active_turn(
    home: &HomeLayout,
    group: &GroupDoc,
    actor: &Actor,
    runtime: &Value,
    c: &mut Value,
) -> Result<(), io::Error> {
    let execution = &c["dots"]["execution"];
    let canonical = runtime["active_turn_id"]
        .as_str()
        .is_some_and(|id| !id.is_empty());
    let turn_id = if canonical {
        &runtime["active_turn_id"]
    } else {
        &execution["turn_id"]
    }
    .as_str()
    .unwrap_or_default()
    .to_owned();
    let sources = if canonical {
        &runtime["active_event_ids"]
    } else {
        &execution["event_ids"]
    };
    let event_ids: Vec<String> = sources
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
        let completion = match super::super::runtime_completion::committed(
            home,
            &group.group_id,
            &actor.id,
            &turn_id,
            &event_ids,
        )
        .map_err(|_| io::Error::other("dots_expire_failed"))?
        {
            Some(committed) => {
                status = committed.status.clone();
                committed
            }
            None => super::super::runtime_completion::Completion {
                turn_id: turn_id.to_owned(),
                event_ids: event_ids.clone(),
                status: status.clone(),
                delivery_id: format!("runtime:{turn_id}"),
            },
        };
        for event_id in &completion.event_ids {
            if let Some((state, transport)) = super::super::runtime_delivery::latest_state(
                home,
                &group.group_id,
                &actor.id,
                event_id,
            )
            .map_err(|_| io::Error::other("dots_expire_failed"))?
                && state == "claimed"
            {
                super::super::runtime_delivery::append_state(
                    home,
                    &group.group_id,
                    &actor.id,
                    &actor.created_at,
                    event_id,
                    &transport,
                    super::super::runtime_delivery::DeliveryOutcome::Ambiguous(
                        "Dots turn expired before completion; its inputs must not be replayed",
                    ),
                )
                .map_err(|_| io::Error::other("dots_expire_failed"))?;
            }
        }
        super::super::runtime_completion::append(home, &group.group_id, &actor.id, &completion)
            .map_err(|_| io::Error::other("dots_expire_failed"))?;
        // This clears canonical ownership only; the private projection below is
        // updated on the same connector snapshot the claim already holds.
        let fresh = super::super::runtime_state::actor_state(home, &group.group_id, &actor.id)
            .map_err(|_| io::Error::other("dots_expire_failed"))?;
        if fresh["active_turn_id"].as_str() == Some(turn_id.as_str()) {
            super::super::runtime_state::set_runtime_status(
                home,
                group,
                &actor.id,
                "waiting",
                "",
                "",
                &[],
            )
            .map_err(|_| io::Error::other("dots_expire_failed"))?;
        }
    }
    let dots = &mut c["dots"];
    let mut last = dots["execution"].clone();
    if last.is_null() {
        // Only the canonical turn identity is known here. Never borrow the
        // pending wake: it can belong to new input and must remain claimable.
        last = json!({"epoch":dots["epoch"],
            "turn_id":turn_id,"event_ids":event_ids,"state":"interrupted"});
    }
    last["state"] = json!("completed");
    last["status"] = json!(status);
    last["completed_at"] = json!(cccc_contracts::utc_now());
    last["epoch"] = dots["epoch"].clone();
    dots["last_execution"] = last;
    dots["execution"] = Value::Null;
    // The pending wake belongs to new input, not the expired turn; it stays
    // deliverable so this same claim can pick it up.
    Ok(())
}
