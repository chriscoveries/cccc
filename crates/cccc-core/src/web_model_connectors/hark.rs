//! Agent-bound, pull-only connectors. The entrance credential can claim work;
//! business tools require a separate capability for the current execution.
use super::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

fn is_hark(c: &Value) -> bool {
    c["kind"] == "hark_connector" && c["provider"] == "hark"
}

pub fn configure(
    home: &HomeLayout,
    group: &str,
    actor: &str,
    generation: &str,
    rotate: bool,
) -> io::Result<Value> {
    if group.is_empty() || actor.is_empty() || generation.is_empty() {
        return Err(error("hark_actor_required"));
    }
    update(home, |root| {
        // A new namespace and kind: existing Web Model credentials never gain
        // pull authority, and each Hark entrance has exactly one immutable route.
        let key = format!("hark:{}", route_key(group, actor));
        let old = &root["connectors"][&key];
        if old.is_object() && old["revoked"] != true && !rotate {
            return Err(error("hark_connector_exists"));
        }
        let now = cccc_contracts::utc_now();
        let mut c = if is_hark(old) && old["revoked"] != true && old["generation"] == generation {
            old.clone()
        } else {
            json!({"connector_id":format!("harkc_{}",Uuid::new_v4().simple()),
                "kind":"hark_connector","provider":"hark","routing_mode":"agent_credential",
                "group_id":group,"actor_id":actor,"generation":generation,"created_at":now,
                "bindings":{},"pairings":{}})
        };
        let secret = format!("harks_{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        c["secret_hash"] = json!(hash(&secret));
        c["secret_preview"] = json!(format!("harks_…{}", &secret[secret.len()-4..]));
        c["routing_key"] = json!(format!("{}{}",Uuid::new_v4().simple(),Uuid::new_v4().simple()));
        c["revoked"] = json!(false);
        c["updated_at"] = json!(now);
        c["bindings"] = json!({});
        c["bindings"][route_key(group, actor)] = json!({"group_id":group,"actor_id":actor,
            "generation":generation,"revision":Uuid::new_v4().to_string(),"transport":"hark_pull",
            "state":"bound","updated_at":now});
        if !c["hark"].is_object() {
            c["hark"] = json!({"execution":null,"last_execution":null});
        }
        c["hark"]["epoch"] = json!(Uuid::new_v4().to_string());
        if c["hark"]["execution"].is_object() {
            c["hark"]["execution"]["state"] = json!("interrupted");
        }
        root["connectors"][key] = c.clone();
        Ok(json!({"connector":c,"secret":secret}))
    })
}

pub fn state<'a>(c: &'a Value, group: &str, actor: &str) -> &'a Value {
    if is_hark(c) && c["group_id"] == group && c["actor_id"] == actor {
        &c["hark"]
    } else {
        &Value::Null
    }
}

pub fn change<T>(home: &HomeLayout, id: &str, group: &str, actor: &str,
    change: impl FnOnce(&mut Value, &mut Value) -> io::Result<T>) -> io::Result<T> {
    update(home, |root| {
        let c = current(root, id)?;
        if !is_hark(c) || c["group_id"] != group || c["actor_id"] != actor {
            return Err(error("hark_binding_required"));
        }
        let mut s = c["hark"].clone();
        let result = change(c, &mut s)?;
        c["hark"] = s;
        Ok(result)
    })
}

pub fn token(c: &Value, b: &Value, s: &Value, turn: &str) -> io::Result<String> {
    let mac = binding_mac(c, b, "cccc-hark-execution-v1", &[s["epoch"].clone(), json!(turn)])?;
    Ok(URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

pub fn resolve(c: &Value, supplied: &str, tool: &str) -> Option<Value> {
    if !is_hark(c) || c["revoked"] == true {
        return None;
    }
    let group = c["group_id"].as_str()?;
    let actor = c["actor_id"].as_str()?;
    let mut b = binding_for_actor(c, group, actor, c["generation"].as_str()?)?;
    if b["transport"] != "hark_pull" { return None; }
    let s = state(c, group, actor);
    b["hark_epoch"] = s["epoch"].clone();
    if supplied.is_empty() && matches!(tool, "cccc_runtime_wait_next_turn" | "cccc_connector_status") {
        b["hark_stage"] = json!("claim");
        return Some(b);
    }
    if supplied.len() != 43 || tool == "cccc_runtime_wait_next_turn" { return None; }
    let bytes = URL_SAFE_NO_PAD.decode(supplied).ok()?;
    for (stage, e) in [("execution", &s["execution"]), ("completion", &s["last_execution"])] {
        if stage == "completion" && tool != "cccc_runtime_complete_turn" { continue; }
        if (stage == "execution" && e["state"] != "active")
            || (stage == "completion" && e["state"] != "completed") { continue; }
        let Some(turn) = e["turn_id"].as_str().filter(|t| !t.is_empty()) else { continue; };
        let mac = binding_mac(c, &b, "cccc-hark-execution-v1", &[s["epoch"].clone(),json!(turn)]).ok()?;
        if mac.verify_slice(&bytes).is_ok() {
            b["hark_stage"] = json!(stage);
            b["hark_turn_id"] = json!(turn);
            return Some(b);
        }
    }
    None
}

pub fn validate(c: &Value, b: &Value, group: &crate::GroupDoc) -> io::Result<()> {
    if !is_hark(c) || c["revoked"] == true || c["group_id"] != b["group_id"] || c["actor_id"] != b["actor_id"] {
        return Err(error("hark_binding_required"));
    }
    let s = state(c, b["group_id"].as_str().unwrap_or(""), b["actor_id"].as_str().unwrap_or(""));
    if s["epoch"] != b["hark_epoch"] || !s["epoch"].is_string() { return Err(error("hark_task_expired")); }
    // A stopped actor can observe stopped/unfinished status, never execute.
    if b["hark_stage"] == "claim" { return Ok(()); }
    if !group.running || matches!(group.state, cccc_contracts::GroupState::Paused | cccc_contracts::GroupState::Stopped) {
        return Err(error("hark_task_expired"));
    }
    let stage = b["hark_stage"].as_str().unwrap_or("");
    let e = match stage {
        "execution" => &s["execution"],
        "completion" => &s["last_execution"],
        _ => return Err(error("hark_task_required")),
    };
    let valid = e["turn_id"].is_string() && e["turn_id"] == b["hark_turn_id"]
        && if stage == "execution" {
            e["state"] == "active" && group.extra.get("runtime_states")
                .and_then(|r| r.get(b["actor_id"].as_str().unwrap_or("")))
                .is_some_and(|r| r["active_turn_id"] == e["turn_id"])
        } else { e["state"] == "completed" };
    if valid { Ok(()) } else { Err(error("hark_task_expired")) }
}

pub fn invalidate(home: &HomeLayout, group: &str, actor: &str) -> io::Result<()> {
    if !store_path(home).exists() { return Ok(()); }
    update(home, |root| {
        for c in root["connectors"].as_object_mut().expect("map").values_mut()
            .filter(|c| is_hark(c) && c["group_id"] == group && c["actor_id"] == actor) {
            c["hark"]["epoch"] = json!(Uuid::new_v4().to_string());
            if c["hark"]["execution"].is_object() { c["hark"]["execution"]["state"] = json!("interrupted"); }
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests;
