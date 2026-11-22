use cccc_contracts::{Actor, GroupState, RunnerKind};
use cccc_core::{GroupDoc, GroupStore};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

use crate::AppState;

const SUPERVISOR_INTERVAL: Duration = Duration::from_secs(30);
const WARMUP_RETRY_INTERVAL: Duration = Duration::from_secs(30);
const DEFAULT_WIDTH: u32 = 1366;
const DEFAULT_HEIGHT: u32 = 900;

static WARMUP_ATTEMPTS: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();

pub(crate) fn spawn(state: AppState) {
    if state.web_mode.is_read_only() {
        return;
    }
    tokio::spawn(async move {
        let mut events = state.ledger_events.subscribe_global();
        let mut shutdown = state.shutdown.subscribe();
        let mut interval = tokio::time::interval(SUPERVISOR_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.recv() => break,
                _ = interval.tick() => ensure_running_actor(&state, None).await,
                event = events.recv() => match event {
                    Ok(event) => ensure_running_actor(&state, Some(&event.group_id)).await,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        ensure_running_actor(&state, None).await;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    });
}

pub(super) async fn ensure_running_actor(state: &AppState, preferred_group: Option<&str>) {
    use futures_util::{StreamExt, stream};
    stream::iter(running_browser_actors(state, preferred_group))
        .for_each_concurrent(4, |(group_id, actor_id)| {
            ensure_actor(state, group_id, actor_id)
        })
        .await;
}

async fn ensure_actor(state: &AppState, group_id: String, actor_id: String) {
    let session_key = super::web_model_browser::key(&group_id, &actor_id);
    let surface = state.browser_surfaces.info(&session_key).await;
    if surface["active"] == true || reopen_on_demand(state, &group_id, &actor_id, &surface).await {
        prepare_delivery(state, group_id, actor_id).await;
    }
}

/// Shared by the supervisor and the worker that just observed a missing page.
/// The periodic supervisor retries if a wake contends with close or delivery.
pub(super) async fn reopen_on_demand(
    state: &AppState,
    group: &str,
    actor: &str,
    surface: &serde_json::Value,
) -> bool {
    use super::{web_model_delivery::try_browser_work, web_model_delivery_state::target};
    let Ok(Some(_work)) = try_browser_work(state, group, actor) else {
        return false;
    };
    let Some((doc, config)) = load_actor(state, group, actor) else {
        return false;
    };
    if !group_actor_ready_to_start(state, &doc, &config) {
        return false;
    }
    let Ok(target) = target(state, group, actor) else {
        return false;
    };
    if target["kind"] == "existing_chat" {
        if !super::web_model_reopen::has_demand(state, group, actor, &target)
            || super::web_model_reopen::load_recovery_blocks_open(&target)
        {
            return false;
        }
    } else if surface["state"] == "closed" {
        return false;
    }
    let session_key = super::web_model_browser::key(group, actor);
    // Another opener might have replaced the target before we got the permit.
    if state.browser_surfaces.info(&session_key).await["active"] == true {
        return true;
    }
    if !warmup_due(&session_key)
        || super::web_model_reopen::book_recovery_open(state, group, actor, &target).is_err()
    {
        return false;
    }
    if let Err(error) = super::web_model_browser::open_actor_surface(
        state,
        group,
        actor,
        DEFAULT_WIDTH,
        DEFAULT_HEIGHT,
        None,
    )
    .await
    {
        tracing::warn!(%error, group, actor, "Managed web-model browser open failed");
        return false;
    }
    clear_warmup_attempt(&session_key);
    tracing::info!(group, actor, reason = ?surface.get("closed_reason"),
        "Opened managed web-model browser page");
    true
}

async fn prepare_delivery(state: &AppState, group: String, actor: String) {
    if super::web_model_connector_store::for_actor(state, &group, &actor).is_none() {
        super::web_model_pairing::ensure_automatic(state, &group, &actor).await;
    } else {
        super::web_model_delivery::ensure_worker(state.clone(), group, actor).await;
    }
}

pub(super) fn actor_delivery_enabled(state: &AppState, group_id: &str, actor_id: &str) -> bool {
    let Ok(store) = GroupStore::new(state.home.clone()) else {
        return false;
    };
    let Ok(group) = store.load(group_id) else {
        return false;
    };
    let Some(actor) = group.actors.iter().find(|actor| actor.id == actor_id) else {
        return false;
    };
    group_actor_ready_to_start(state, &group, actor)
        && super::web_model_connector_store::for_actor(state, group_id, actor_id).is_some()
}

fn load_actor(state: &AppState, group_id: &str, actor_id: &str) -> Option<(GroupDoc, Actor)> {
    let group = GroupStore::new(state.home.clone())
        .ok()?
        .load(group_id)
        .ok()?;
    let actor = group
        .actors
        .iter()
        .find(|actor| actor.id == actor_id)?
        .clone();
    Some((group, actor))
}

fn running_browser_actors(
    state: &AppState,
    preferred_group: Option<&str>,
) -> Vec<(String, String)> {
    let Ok(store) = GroupStore::new(state.home.clone()) else {
        return Vec::new();
    };
    let groups: Vec<GroupDoc> =
        if let Some(group_id) = preferred_group.filter(|value| !value.is_empty()) {
            store.load(group_id).into_iter().collect()
        } else {
            store
                .list()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|group| store.load(&group.group_id).ok())
                .collect()
        };
    groups
        .into_iter()
        .flat_map(|group| {
            group
                .actors
                .iter()
                .filter(|actor| group_actor_ready_to_start(state, &group, actor))
                .map(|actor| (group.group_id.clone(), actor.id.clone()))
                .collect::<Vec<_>>()
        })
        .collect()
}

fn group_actor_ready_to_start(state: &AppState, group: &GroupDoc, actor: &Actor) -> bool {
    if !actor.runtime.is_browser_web_model()
        || actor.runner != RunnerKind::Headless
        || !actor.enabled
        || !group_state_allows_delivery(group.running, group.state)
    {
        return false;
    }
    let provider = actor_setting(
        actor,
        &["CCCC_WEB_MODEL_PROVIDER", "CCCC_WEB_MODEL_BROWSER_PROVIDER"],
    );
    let provider = if provider.is_empty() {
        super::web_model_connector_store::load(state)
            .ok()
            .and_then(|items| {
                items
                    .into_iter()
                    .find(|c| c["revoked"] != true && c["provider"] == "chatgpt_web")
            })
            .and_then(|connector| connector["provider"].as_str().map(normalize))
            .unwrap_or_default()
    } else {
        provider
    };
    browser_delivery_requested(actor, &provider)
}

fn browser_delivery_requested(actor: &Actor, provider: &str) -> bool {
    let mode = actor_setting(
        actor,
        &["CCCC_WEB_MODEL_DELIVERY_MODE", "CCCC_WEB_MODEL_DELIVERY"],
    );
    if matches!(
        mode.as_str(),
        "pull" | "native" | "remote_mcp" | "off" | "disabled" | "none"
    ) {
        return false;
    }
    if matches!(
        mode.as_str(),
        "browser" | "chatgpt" | "chatgpt_browser" | "browser_delivery"
    ) {
        return true;
    }
    matches!(
        provider,
        "chatgpt" | "chatgpt_web" | "browser_web_model" | "chatgpt_browser"
    )
}

fn actor_setting(actor: &Actor, names: &[&str]) -> String {
    actor_setting_with_process(actor, names, |name| std::env::var(name).ok())
}

fn actor_setting_with_process(
    actor: &Actor,
    names: &[&str],
    mut process_setting: impl FnMut(&str) -> Option<String>,
) -> String {
    names
        .iter()
        .filter_map(|name| actor.env.get(*name).map(normalize))
        .find(|value| !value.is_empty())
        .or_else(|| {
            names
                .iter()
                .filter_map(|name| process_setting(name).map(normalize))
                .find(|value| !value.is_empty())
        })
        .unwrap_or_default()
}

fn group_state_allows_delivery(running: bool, state: GroupState) -> bool {
    running && !matches!(state, GroupState::Paused | GroupState::Stopped)
}

fn normalize(value: impl AsRef<str>) -> String {
    value.as_ref().trim().to_ascii_lowercase()
}

fn warmup_due(key: &str) -> bool {
    let now = Instant::now();
    let attempts = WARMUP_ATTEMPTS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut attempts = attempts
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if attempts
        .get(key)
        .is_some_and(|last| now.duration_since(*last) < WARMUP_RETRY_INTERVAL)
    {
        return false;
    }
    attempts.insert(key.to_owned(), now);
    true
}

fn clear_warmup_attempt(key: &str) {
    let Some(attempts) = WARMUP_ATTEMPTS.get() else {
        return;
    };
    attempts
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(key);
}
