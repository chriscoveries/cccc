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
//! Two deliberate limits. Only managed sessions are enforced by default:
//! PTY idleness is output-silence, not proof of no turn, so a PTY lane
//! restarts only under the explicit `memory_limit_restart_pty` opt-in and
//! only after ten quiet minutes — and that restart is a FRESH session, since
//! PTY lanes have no resume. A restart is only attempted for a currently
//! running session whose provider root is ownership-verified (recorded pid
//! plus start-time identity, rechecked at admission) — never a blind
//! fresh start.

use std::collections::HashMap;
use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cccc_contracts::{Event, RuntimeStateSource};
use cccc_core::{GroupDoc, HomeLayout};
use cccc_runtime::ActivitySnapshot;
use serde_json::{Value, json};

use super::actor_runtime_status;
use super::actor_memory_sampling as sampling;

/// Minimum seconds between restarts of one actor, even across daemon
/// restarts. The marker is written *before* the restart is attempted, so a
/// crash mid-restart still holds the window (fail quiet, never hot-loop).
const RESTART_COOLDOWN_SECS: u64 = 1800;

/// Output-silence window that counts as PTY-quiet. A lane that produced no
/// output for this long, with no newer input waiting, is treated as idle —
/// with the explicit caveat (documented on the setting) that silence is not
/// proof of no turn, which is why this path is opt-in and restarts fresh.
const PTY_QUIET_SECS: u64 = 600;

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
    let body = serde_json::to_string_pretty(&json!({"restarts": marks}))
        .map_err(io::Error::other)?;
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

/// PTY-quiet test over a terminal activity snapshot: output silent for the
/// whole window, and no input newer than that output (an unanswered delivery
/// may still be running). Fresh sessions with no output yet are NOT quiet —
/// silence must be observed, not assumed.
pub fn pty_quiet_for(snapshot: &ActivitySnapshot, quiet: Duration) -> bool {
    let Some(output) = &snapshot.last_output else {
        return false;
    };
    if output.age < quiet {
        return false;
    }
    match &snapshot.last_input {
        None => true,
        Some(input) => input.age >= output.age,
    }
}

/// Live PTY-quiet check for an actor. False on any error (no session, lock
/// poison): an unreadable tracker never authorises a restart.
fn pty_quiet_live(group_id: &str, actor_id: &str) -> bool {
    cccc_runtime::activity(group_id, actor_id)
        .map(|snapshot| pty_quiet_for(&snapshot, Duration::from_secs(PTY_QUIET_SECS)))
        .unwrap_or(false)
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
        if marks.get(&actor.id).is_some_and(|at| now < at.saturating_add(RESTART_COOLDOWN_SECS)) {
            continue;
        }
        // Idle first (cheap snapshots); sampling only happens for idle lanes.
        // Managed sessions gate on no-turn-and-idle; PTY lanes gate on the
        // explicit opt-in plus output silence (fresh restart, no resume).
        let idle_managed = managed_idle(&group.group_id, &actor.id);
        let idle_pty = !idle_managed
            && actor.memory_limit_restart_pty
            && actor.runtime_state_source == RuntimeStateSource::Terminal
            && pty_quiet_live(&group.group_id, &actor.id);
        if !idle_managed && !idle_pty {
            continue;
        }
        let fresh_restart = !idle_managed;
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
        if fresh_restart {
            if !(fresh_actor.memory_limit_restart_pty
                && fresh_actor.runtime_state_source == RuntimeStateSource::Terminal
                && pty_quiet_live(&group.group_id, &actor.id))
            {
                continue;
            }
        } else if !managed_idle(&group.group_id, &actor.id) {
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
                if fresh_restart {
                    "fresh session: PTY lanes have no resume."
                } else {
                    "session resumed by actor.restart."
                },
            )?),
            Err(error) => out.push(emit(
                home,
                &group.group_id,
                &actor.id,
                limit_mb as u64,
                rss_mb,
                "restart_failed",
                &format!("actor.restart refused ({}): {}. Cooldown holds; will not retry hot.", error.code, error.message),
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
        // ... and a PTY actor with a limit but WITHOUT the opt-in is never
        // restarted either: the opt-in defaults off.
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
}

#[cfg(test)]
mod pty_quiet_tests {
    use super::*;
    use std::time::Duration;

    fn snapshot(out_age_secs: Option<u64>, in_age_secs: Option<u64>) -> ActivitySnapshot {
        let instant = |age: u64| cccc_runtime::ActivityInstant {
            at: SystemTime::now(),
            age: Duration::from_secs(age),
        };
        ActivitySnapshot {
            last_output: out_age_secs.map(instant),
            last_input: in_age_secs.map(instant),
            recent_output: Vec::new(),
            output_bytes_since_input: 0,
        }
    }

    const QUIET: Duration = Duration::from_secs(PTY_QUIET_SECS);

    #[test]
    fn quiet_needs_old_output_and_no_newer_input() {
        // Old output, older input: quiet.
        assert!(pty_quiet_for(&snapshot(Some(1200), Some(1300)), QUIET));
        // Old output, no input at all: quiet.
        assert!(pty_quiet_for(&snapshot(Some(1200), None), QUIET));
        // Recent output: working, however old the input.
        assert!(!pty_quiet_for(&snapshot(Some(60), Some(3600)), QUIET));
        // Input newer than the last output: a delivery may be running.
        assert!(!pty_quiet_for(&snapshot(Some(1200), Some(60)), QUIET));
        // Never any output: silence unobserved, not quiet.
        assert!(!pty_quiet_for(&snapshot(None, None), QUIET));
        assert!(!pty_quiet_for(&snapshot(None, Some(5000)), QUIET));
        // Boundary: exactly the window counts as quiet.
        assert!(pty_quiet_for(&snapshot(Some(600), Some(600)), QUIET));
    }

    #[test]
    fn fresh_session_is_not_quiet() {
        // A live PTY session that produced no output yet must not read as
        // quiet: quietness is observed silence, and there is nothing to
        // observe yet.
        assert!(!pty_quiet_live("no-such-group", "no-such-actor"));
    }

    #[test]
    fn pty_opt_in_round_trips_and_defaults_off() {
        let plain =
            serde_json::to_value(cccc_contracts::Actor::new("pty-worker")).expect("serialize");
        assert!(plain.get("memory_limit_restart_pty").is_none());
        let mut configured = plain;
        configured["memory_limit_restart_pty"] = serde_json::json!(true);
        let actor: cccc_contracts::Actor =
            serde_json::from_value(configured).expect("configured actor");
        assert!(actor.memory_limit_restart_pty);
        assert_eq!(
            serde_json::to_value(actor).expect("serialize")["memory_limit_restart_pty"],
            serde_json::json!(true)
        );
    }
}
