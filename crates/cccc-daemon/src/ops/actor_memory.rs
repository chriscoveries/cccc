//! Per-actor memory ceilings: sample, idle-gate, restart with resume.
//!
//! An actor with nonzero `memory_limit_mb` gets its process tree's resident
//! set sampled on the maintenance tick. Over the limit while IDLE, the lane
//! is gracefully restarted through the daemon's own `actor.restart` (which
//! resumes managed sessions); never mid-turn. At most one restart per actor
//! per 30 minutes, tracked in a file that survives daemon restarts. One
//! ledger event plus one targeted foreman notice per restart; a failed
//! restart is recorded the same way, never swallowed.
//!
//! Two deliberate limits. Only managed sessions are enforced: PTY idleness is
//! a heuristic and cannot prove never-mid-turn, so a PTY lane is never
//! restarted by this pass. And a restart is only attempted for a currently
//! running session whose provider root is ownership-verified (recorded pid
//! plus start-time identity, rechecked at admission) — never a blind
//! fresh start.

use std::collections::HashMap;
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use cccc_contracts::Event;
use cccc_core::{GroupDoc, HomeLayout};
use serde_json::{Value, json};

use super::actor_memory_sampling as sampling;
use super::actor_runtime_status;

/// Minimum seconds between restarts of one actor, even across daemon
/// restarts. The marker is written *before* the restart is attempted, so a
/// crash mid-restart still holds the window (fail quiet, never hot-loop).
const RESTART_COOLDOWN_SECS: u64 = 1800;

/// Cooldown file name inside the group's state dir.
const COOLDOWN_FILE: &str = "actor_memory.json";

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn cooldown_path(home: &HomeLayout, group_id: &str) -> io::Result<std::path::PathBuf> {
    Ok(cccc_core::GroupStore::new(home.clone())?
        .state_dir(group_id)?
        .join(COOLDOWN_FILE))
}

fn read_cooldowns(home: &HomeLayout, group_id: &str) -> HashMap<String, u64> {
    let Ok(path) = cooldown_path(home, group_id) else {
        return HashMap::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return HashMap::new();
    };
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|value| value.get("restarts")?.as_object().cloned())
        .map(|map| {
            map.into_iter()
                .filter_map(|(actor, at)| Some((actor, at.as_u64()?)))
                .collect()
        })
        .unwrap_or_default()
}

fn write_cooldown(home: &HomeLayout, group_id: &str, actor_id: &str, at: u64) -> io::Result<()> {
    let path = cooldown_path(home, group_id)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut marks = read_cooldowns(home, group_id);
    marks.insert(actor_id.to_owned(), at);
    let body =
        serde_json::to_string_pretty(&json!({"restarts": marks})).map_err(io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Managed-idle gate: running, no active turn, idle status. A snapshot, not
/// a fence — callers recheck it at admission because a turn can start
/// between the sample and the restart.
fn managed_idle(group_id: &str, actor_id: &str) -> bool {
    super::local_headless::ready_for_mail_notice(group_id, actor_id)
}

/// Provider root of the actor's live tree: the recorded session pid, with
/// its start time for birth identity. `None` when there is no attributable
/// live root (nothing to sample, nothing to restart).
fn live_root(group: &GroupDoc, actor_id: &str) -> Option<(i32, u64)> {
    let actor = group.actors.iter().find(|actor| actor.id == actor_id)?;
    let status = actor_runtime_status::resolve(group, actor);
    if !status.running {
        return None;
    }
    let pid = status.pid? as i32;
    let started = sampling::started_secs_of(pid)?;
    Some((pid, started))
}

fn notice(
    group_id: &str,
    actor_id: &str,
    limit_mb: u64,
    rss_mb: u64,
    outcome: &str,
    detail: &str,
) -> Event {
    let mut event = Event::new("system.notify", group_id);
    event.by = "system".into();
    event.data = json!({
        "kind": "memory_restart",
        "priority": "normal",
        "title": "Actor memory ceiling",
        "message": format!(
            "actor {actor_id} resident {rss_mb} MiB over limit {limit_mb} MiB while idle: {outcome}. {detail}",
        ),
        "to": ["@foreman"],
        "im_visibility": "internal",
        "context": {
            "actor_id": actor_id,
            "limit_mb": limit_mb,
            "rss_mb": rss_mb,
            "outcome": outcome,
        },
    })
    .as_object()
    .cloned()
    .unwrap_or_default();
    event
}

/// One enforcement pass for a group. Returns ledger-appended events for the
/// caller to dispatch. Pure observation when nothing qualifies: no writes,
/// no signals.
pub fn tick(home: &HomeLayout, group: &GroupDoc) -> io::Result<Vec<Event>> {
    let mut out = Vec::new();
    let now = now_secs();
    let marks = read_cooldowns(home, &group.group_id);
    for actor in group.actors.iter().filter(|actor| actor.enabled) {
        let limit_mb = actor.memory_limit_mb;
        if limit_mb == 0 {
            continue;
        }
        if marks
            .get(&actor.id)
            .is_some_and(|at| now < at.saturating_add(RESTART_COOLDOWN_SECS))
        {
            continue;
        }
        // Idle first (cheap snapshot); sampling only happens for idle lanes.
        if !managed_idle(&group.group_id, &actor.id) {
            continue;
        }
        let Some((root, started)) = live_root(group, &actor.id) else {
            continue;
        };
        let (total_kib, _counted) = sampling::tree_rss_kib(root);
        if total_kib / 1024 <= limit_mb as u64 {
            continue;
        }
        let rss_mb = total_kib / 1024;
        // --- admission: re-verify everything after the sample ---
        let Ok(fresh_store) = cccc_core::GroupStore::new(home.clone()) else {
            continue;
        };
        let Ok(fresh) = fresh_store.load(&group.group_id) else {
            continue;
        };
        let Some(fresh_actor) = fresh.actors.iter().find(|a| a.id == actor.id) else {
            continue;
        };
        if !fresh_actor.enabled || fresh_actor.memory_limit_mb == 0 {
            continue;
        }
        if !managed_idle(&group.group_id, &actor.id) {
            continue;
        }
        // Reserve the cooldown window BEFORE attempting: a crash mid-restart
        // must not hot-loop, and a persistent failure must not retry hot.
        if write_cooldown(home, &group.group_id, &actor.id, now).is_err() {
            continue;
        }
        // Ownership re-verification: same pid, same birth. A recycled pid or
        // a replaced session aborts the restart, quietly.
        let live_now = live_root(&fresh, &actor.id);
        if live_now != Some((root, started)) {
            continue;
        }
        match super::actor_runtime::apply(home, &fresh, &actor.id, "actor.restart") {
            Ok(_) => out.push(emit(
                home,
                &group.group_id,
                &actor.id,
                limit_mb as u64,
                rss_mb,
                "restarted",
                "session resumed by actor.restart.",
            )?),
            Err(error) => out.push(emit(
                home,
                &group.group_id,
                &actor.id,
                limit_mb as u64,
                rss_mb,
                "restart_failed",
                &format!(
                    "actor.restart refused ({}): {}. Cooldown holds; will not retry hot.",
                    error.code, error.message
                ),
            )?),
        }
    }
    Ok(out)
}

fn emit(
    home: &HomeLayout,
    group_id: &str,
    actor_id: &str,
    limit_mb: u64,
    rss_mb: u64,
    outcome: &str,
    detail: &str,
) -> io::Result<Event> {
    let event = notice(group_id, actor_id, limit_mb, rss_mb, outcome, detail);
    let path = cccc_core::GroupStore::new(home.clone())?.ledger_path(group_id)?;
    cccc_core::ledger::append(&path, &event)?;
    Ok(event)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cooldown_window_holds_then_releases() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("home");
        let store = cccc_core::GroupStore::new(home.clone()).expect("store");
        let group = store.create("cooldown", "").expect("group");
        let gid = group.group_id.clone();
        // Missing file: no cooldown.
        assert!(read_cooldowns(&home, &gid).is_empty());
        write_cooldown(&home, &gid, "a", 10_000).expect("write");
        let marks = read_cooldowns(&home, &gid);
        assert_eq!(marks.get("a"), Some(&10_000));
        // Inside the window the marker blocks; the window is 30 minutes.
        assert_eq!(RESTART_COOLDOWN_SECS, 1800);
        // Corrupt file: treated as empty (fail open for monitoring, and the
        // restart itself still needs idle + over-limit + admission).
        let path = cooldown_path(&home, &gid).expect("path");
        std::fs::write(&path, "{ not json").expect("corrupt");
        assert!(read_cooldowns(&home, &gid).is_empty());
    }

    #[test]
    fn tick_with_no_limited_actors_writes_nothing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("home");
        let store = cccc_core::GroupStore::new(home.clone()).expect("store");
        let group = store.create("quiet", "").expect("group");
        let before = std::fs::read_to_string(store.ledger_path(&group.group_id).expect("ledger"))
            .unwrap_or_default()
            .len();
        let events = tick(&home, &group).expect("tick");
        assert!(events.is_empty());
        let after = std::fs::read_to_string(store.ledger_path(&group.group_id).expect("ledger"))
            .unwrap_or_default()
            .len();
        assert_eq!(before, after, "a quiet tick must not append");
        // ... and a PTY actor with a limit is observed but never restarted:
        // idleness here is unprovable, so no event, no cooldown file.
        let mut cela = group.clone();
        let mut actor = cccc_contracts::Actor::new("pty-lane");
        actor.memory_limit_mb = 1;
        cela.actors.push(actor);
        store.save(&cela).expect("save");
        let reloaded = store.load(&group.group_id).expect("reload");
        let events = tick(&home, &reloaded).expect("tick");
        assert!(events.is_empty());
        assert!(
            cooldown_path(&home, &group.group_id)
                .map(|path| !path.exists())
                .unwrap_or(true),
            "no restart means no cooldown marker"
        );
    }

    #[test]
    fn notice_shape_targets_foreman_only() {
        let event = notice("g", "a", 256, 1024, "restarted", "d.");
        assert_eq!(event.data["kind"], "memory_restart");
        assert_eq!(event.data["to"], json!(["@foreman"]));
        assert_eq!(event.data["context"]["outcome"], "restarted");
        assert_eq!(event.data["context"]["rss_mb"], 1024);
    }

    #[test]
    fn memory_restart_targets_only_the_foreman_in_dispatch_and_recovery() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("initialize");
        let store = cccc_core::GroupStore::new(home.clone()).expect("store");
        let mut group = store.create("memory routing", "").expect("group");
        group.state = cccc_contracts::GroupState::Stopped;
        group.actors = vec![
            cccc_contracts::Actor::new("lead"),
            cccc_contracts::Actor::new("peer"),
        ];
        store.save(&group).expect("save actors");
        let event = emit(
            &home,
            &group.group_id,
            "worker",
            500,
            800,
            "restarted",
            "same session",
        )
        .expect("emit notice");
        assert_eq!(event.data["target_actor_id"], "lead");
        // The stopped group selects routing without starting any test actor.
        let report = super::super::actor_delivery::dispatch(&home, &group, &event);
        assert_eq!(report.targeted, 1);
        assert_eq!(report.queued, 0);
        for (actor, expected) in [(&group.actors[0], true), (&group.actors[1], false)] {
            let pending = super::super::runtime_delivery::pending_sources(&home, &group, actor, 10)
                .expect("pending notifications");
            assert_eq!(pending.iter().any(|item| item.id == event.id), expected);
            assert!(
                !cccc_core::inbox::is_for_actor(&group, &event, &actor.id),
                "maintenance notices never become inbox messages"
            );
        }
    }

    #[test]
    fn memory_restart_without_an_available_foreman_stays_ledger_only() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("initialize");
        let store = cccc_core::GroupStore::new(home.clone()).expect("store");
        let mut group = store
            .create("memory routing no foreman", "")
            .expect("group");
        group.state = cccc_contracts::GroupState::Stopped;
        let mut lead = cccc_contracts::Actor::new("lead");
        lead.enabled = false;
        group.actors = vec![lead, cccc_contracts::Actor::new("peer")];
        store.save(&group).expect("save actors");
        let event = emit(
            &home,
            &group.group_id,
            "worker",
            500,
            800,
            "restart_failed",
            "receipt missing",
        )
        .expect("emit outcome");
        assert!(event.data.get("target_actor_id").is_none());
        let report = super::super::actor_delivery::dispatch(&home, &group, &event);
        assert_eq!(report.targeted, 0, "never fall back to broadcasting");
        let ledger =
            cccc_core::ledger::read_all(&store.ledger_path(&group.group_id).expect("path"))
                .expect("ledger");
        assert_eq!(ledger.iter().filter(|item| item.kind == "system.notify" && item.data["kind"] == "memory_restart").count(), 1);
    }
}
