use crate::dispatch::OpError;
use crate::ops::{actor_profile_runtime, actor_secrets};
use cccc_contracts::{Actor, ActorRuntime};
use cccc_core::{GroupDoc, HomeLayout};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

#[cfg(test)]
mod snapshot_tests;

pub(super) fn resolve_launch_actor(
    home: &HomeLayout,
    group: &GroupDoc,
    actor: &Actor,
) -> Result<Actor, OpError> {
    let mut actor = actor_profile_runtime::resolve(home, actor)?;
    actor.env = actor_secrets::effective_values(home, &group.group_id, &actor)?;
    Ok(actor)
}

pub(super) fn launch_env(
    home: &HomeLayout,
    group: &GroupDoc,
    actor: &Actor,
) -> Result<BTreeMap<String, String>, OpError> {
    let mut env = actor.env.clone();
    if let Some(key) = snapshot_config_key(actor.runtime) {
        let inherited = std::env::var(key).ok();
        merge_snapshot_config(&mut env, key, inherited.as_deref(), false)?;
    }
    // Packaged/test installations need the same CLI as their owning daemon,
    // even when that executable is not installed on the user's shell PATH.
    crate::ops::codex_mcp::configure_actor_cli(&mut env);
    env.insert(
        "CCCC_HOME".into(),
        home.root().to_string_lossy().into_owned(),
    );
    env.insert("CCCC_GROUP_ID".into(), group.group_id.clone());
    env.insert("CCCC_ACTOR_ID".into(), actor.id.clone());
    Ok(env)
}

fn snapshot_config_key(runtime: ActorRuntime) -> Option<&'static str> {
    match runtime {
        ActorRuntime::Opencode => Some("OPENCODE_CONFIG_CONTENT"),
        ActorRuntime::Kilo => Some("KILO_CONFIG_CONTENT"),
        _ => None,
    }
}

fn merge_snapshot_config(
    env: &mut BTreeMap<String, String>,
    key: &str,
    inherited: Option<&str>,
    snapshot: bool,
) -> Result<(), OpError> {
    let actor_override = env.contains_key(key);
    let content = env.get(key).map(String::as_str).or(inherited);
    let mut config = match content.filter(|content| !content.trim().is_empty()) {
        Some(content) => serde_json::from_str::<Value>(content)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .ok_or_else(|| {
                // Inline configuration may contain credentials. Do not include
                // the value or parser diagnostics in an externally visible error.
                OpError::new(
                    "invalid_runtime_config",
                    format!(
                        "{key} must contain a JSON object; existing configuration was not replaced"
                    ),
                )
            })?,
        None => Map::new(),
    };
    if actor_override {
        // Actor/profile environment is more specific than CCCC's default.
        config.entry("snapshot").or_insert(Value::Bool(snapshot));
    } else {
        // Preserve inherited provider/MCP configuration while applying the
        // launch policy over a daemon-wide snapshot choice.
        config.insert("snapshot".into(), Value::Bool(snapshot));
    }
    env.insert(key.into(), Value::Object(config).to_string());
    Ok(())
}
