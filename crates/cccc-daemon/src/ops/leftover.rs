//! Leftover lane processes: detection, classification, notice, opt-in reap.
//!
//! Every actor runtime is launched with `CCCC_GROUP_ID` / `CCCC_ACTOR_ID` in
//! its environment. The abrupt-exit guard reaps recorded process *groups* when
//! their owner dies, but a process can still escape that net: reparented out
//! of its group, spawned before the guard existed, or left behind by a
//! watchdog that was itself killed. Those strays keep their `CCCC_*`
//! environment, so [`cccc_runtime::scan_tagged_processes`] finds them even
//! though no owner record points at them.
//!
//! Classification joins each tagged process against live daemon state:
//!
//! - `group_gone` — its group id names no group in this home;
//! - `actor_gone` — the group exists but has no such actor;
//! - `stale_generation` — the actor exists but the process started before its
//!   latest session start, so a restart replaced whatever owned it;
//! - `actor_stopped` — the actor exists and is not running.
//!
//! A process of a running actor that started after that actor's latest session
//! start is live, not leftover, and is never listed. Unattributable processes
//! (the scan only returns tagged ones) never reach the reaper by construction.
//!
//! Surfacing is threefold: the read-only `leftover_processes` op (used by
//! `cccc doctor` and the Web UI), at most one `system.notify` to the group
//! foreman per new batch, and the opt-in `runtime.reap_leftover_processes`
//! setting (default off) which TERMs, then KILLs after a grace period, only
//! leftovers older than `runtime.reap_leftover_after_hours` (default 24).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cccc_contracts::{DaemonRequest, Event};
use cccc_core::{GroupDoc, HomeLayout};
use serde_json::{Value, json};

use super::actor_runtime_status;
use super::operation::{Operation, Policy::Read};
use crate::dispatch::{OpResult, object};
use cccc_runtime::TaggedProcess;

pub(super) fn resolve_operation(request: &DaemonRequest) -> Option<Operation> {
    Some(match request.op.as_str() {
        "leftover_processes" => Operation::new(Read, list),
        _ => return None,
    })
}

/// Seconds of grace between TERM and KILL when the opt-in reaper acts.
/// Short enough to keep the tick moving, long enough for a lane process to
/// trap TERM and exit cleanly.
const REAP_GRACE: Duration = Duration::from_secs(15);

/// One classified stray.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leftover {
    pub pid: i32,
    pub pgid: i32,
    pub comm: String,
    pub actor_id: String,
    pub group_id: String,
    pub class: &'static str,
    pub started_secs: u64,
    pub age_secs: u64,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn parse_ts(value: &str, fallback: i64) -> i64 {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|value| value.timestamp())
        .unwrap_or(fallback)
}

/// Latest session-start timestamp per actor from the group ledger
/// (`actor.start`, `actor.restart`, `actor.new_session`). A restart replaces
/// whatever owned the actor's old processes, so anything older is stale.
fn session_starts(events: &[Event]) -> HashMap<String, i64> {
    let mut starts = HashMap::<String, i64>::new();
    for event in events {
        if !matches!(
            event.kind.as_str(),
            "actor.start" | "actor.restart" | "actor.new_session"
        ) {
            continue;
        }
        let Some(actor_id) = event.data.get("actor_id").and_then(Value::as_str) else {
            continue;
        };
        let at = parse_ts(&event.ts, 0);
        starts
            .entry(actor_id.to_owned())
            .and_modify(|seen| *seen = (*seen).max(at))
            .or_insert(at);
    }
    starts
}

fn is_running(group: &GroupDoc, actor_id: &str) -> bool {
    group
        .actors
        .iter()
        .find(|actor| actor.id == actor_id)
        .is_some_and(|actor| actor_runtime_status::resolve(group, actor).running)
}

/// Classify one tagged process. `None` means live (or ours): not a leftover.
fn classify(
    process: &TaggedProcess,
    groups: &HashMap<String, GroupDoc>,
    starts: &HashMap<String, HashMap<String, i64>>,
    own_pid: i32,
    own_pgid: i32,
) -> Option<&'static str> {
    if process.pid <= 1 || process.pid == own_pid {
        return None;
    }
    // Never anything in the live daemon's own process group: that tree is
    // ours by construction, whatever tags it carries.
    if process.pgid > 1 && process.pgid == own_pgid {
        return None;
    }
    let Some(group) = groups.get(&process.group_id) else {
        return Some("group_gone");
    };
    let Some(actor) = group.actors.iter().find(|actor| actor.id == process.actor_id) else {
        return Some("actor_gone");
    };
    let running = actor_runtime_status::resolve(group, actor).running;
    let latest_start = starts
        .get(&process.group_id)
        .and_then(|starts| starts.get(&process.actor_id))
        .copied()
        .unwrap_or(0);
    if running {
        // A running actor's current session owns everything started after its
        // latest session start. Older processes predate a restart: stale.
        if latest_start > 0 && process.started_secs < latest_start as u64 {
            return Some("stale_generation");
        }
        return None;
    }
    // Not running: the session this process belonged to is gone, whatever
    // its timestamps say. Running state is checked again at reap time.
    Some("actor_stopped")
}

/// Scan the host and classify every tagged process. Pure host read: no ledger
/// writes, no signals.
pub fn check(home: &HomeLayout) -> Vec<Leftover> {
    let own_pid = std::process::id() as i32;
    let own_pgid = pgid_of(own_pid);
    let tagged = cccc_runtime::scan_tagged_processes();
    if tagged.is_empty() {
        return Vec::new();
    }
    let Ok(store) = cccc_core::GroupStore::new(home.clone()) else {
        return Vec::new();
    };
    let group_ids = cccc_core::automation::group_ids(home).unwrap_or_default();
    let mut groups = HashMap::new();
    for group_id in &group_ids {
        if let Ok(group) = store.load(group_id) {
            groups.insert(group_id.clone(), group);
        }
    }
    // Session starts per group, read once up front.
    let mut starts = HashMap::new();
    for group_id in groups.keys() {
        let Ok(path) = store.ledger_path(group_id) else {
            continue;
        };
        let events = cccc_core::ledger::read_all(&path).unwrap_or_default();
        starts.insert(group_id.clone(), session_starts(&events));
    }
    let now = now_secs();
    let mut leftovers = Vec::new();
    for process in &tagged {
        let Some(class) = classify(process, &groups, &starts, own_pid, own_pgid) else {
            continue;
        };
        leftovers.push(Leftover {
            pid: process.pid,
            pgid: process.pgid,
            comm: process.comm.clone(),
            actor_id: process.actor_id.clone(),
            group_id: process.group_id.clone(),
            class,
            started_secs: process.started_secs,
            // Unknown start (0, e.g. a macOS fallback hit) must never read
            // as ancient: such a process is listed and notified, but the age
            // gate below keeps it out of the reaper.
            age_secs: if process.started_secs == 0 {
                0
            } else {
                now.saturating_sub(process.started_secs)
            },
        });
    }
    leftovers.sort_by_key(|leftover| (leftover.group_id.clone(), leftover.actor_id.clone(), leftover.pid));
    leftovers
}

#[cfg(unix)]
fn pgid_of(pid: i32) -> i32 {
    // Same field the scan parses (`stat` field 4, pgrp); no subprocess, no
    // new dependency. Failure degrades to 0, which matches nothing tagged.
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            let after = stat.rsplit(')').next()?;
            let mut fields = after.split_whitespace();
            fields.next()?;
            fields.next().and_then(|field| field.parse::<i32>().ok())
        })
        .unwrap_or(0)
}

#[cfg(not(unix))]
fn pgid_of(_pid: i32) -> i32 {
    0
}

fn leftover_json(leftover: &Leftover) -> Value {
    json!({
        "pid": leftover.pid,
        "pgid": leftover.pgid,
        "comm": leftover.comm,
        "actor_id": leftover.actor_id,
        "group_id": leftover.group_id,
        "class": leftover.class,
        "started_secs": leftover.started_secs,
        "age_secs": leftover.age_secs,
    })
}

fn list(home: &HomeLayout, request: &DaemonRequest) -> OpResult {
    let group_id = request
        .args
        .get("group_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let leftovers: Vec<Leftover> = check(home)
        .into_iter()
        .filter(|leftover| group_id.is_empty() || leftover.group_id == group_id)
        .collect();
    object(json!({
        "leftovers": leftovers.iter().map(leftover_json).collect::<Vec<_>>(),
        "count": leftovers.len(),
    }))
}

/// Fingerprint of a leftover batch: one triple per (group, actor, class).
/// Pids are deliberately excluded — they recycle, and a notice is about the
/// attribution, not the process table row.
fn batch_key(leftovers: &[Leftover]) -> BTreeSet<String> {
    leftovers
        .iter()
        .map(|leftover| {
            format!(
                "{}:{}:{}",
                leftover.group_id, leftover.actor_id, leftover.class
            )
        })
        .collect()
}

/// Triples already notified in this group's ledger.
fn notified_triples(home: &HomeLayout, group_id: &str) -> HashSet<String> {
    let Ok(store) = cccc_core::GroupStore::new(home.clone()) else {
        return HashSet::new();
    };
    let Ok(path) = store.ledger_path(group_id) else {
        return HashSet::new();
    };
    let events = cccc_core::ledger::read_all(&path).unwrap_or_default();
    let mut seen = HashSet::new();
    for event in &events {
        if event.kind != "system.notify" {
            continue;
        }
        if event.data.get("kind").and_then(Value::as_str) != Some("leftover_batch") {
            continue;
        }
        if let Some(triples) = event
            .data
            .get("context")
            .and_then(|context| context.get("triples"))
            .and_then(Value::as_array)
        {
            seen.extend(triples.iter().filter_map(Value::as_str).map(str::to_owned));
        }
    }
    seen
}

fn read_bool(group: &GroupDoc, section: &str, key: &str) -> bool {
    group
        .extra
        .get(section)
        .and_then(|section| section.get(key))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn read_hours(group: &GroupDoc) -> u64 {
    group
        .extra
        .get("runtime")
        .and_then(|section| section.get("reap_leftover_after_hours"))
        .and_then(Value::as_u64)
        .unwrap_or(24)
}

/// One maintenance pass for a group: notify the foreman about new leftover
/// batches (at most one notice per new batch), and, when the opt-in setting
/// is on, reap what is old enough.
///
/// Only strays tagged with this group's id are considered here. A `group_gone`
/// stray belongs to no group, so no foreman can be notified about it; those
/// stay visible through the `leftover_processes` op (and `cccc doctor`), which
/// is group-scoped only when asked to be.
pub fn tick(home: &HomeLayout, group: &GroupDoc) -> io::Result<Vec<Event>> {
    let leftovers: Vec<Leftover> = check(home)
        .into_iter()
        .filter(|leftover| leftover.group_id == group.group_id)
        .collect();
    if leftovers.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let key = batch_key(&leftovers);
    let seen = notified_triples(home, &group.group_id);
    let fresh: BTreeSet<String> = key
        .iter()
        .filter(|triple| !seen.contains(triple.as_str()))
        .cloned()
        .collect();
    if !fresh.is_empty() {
        let fresh_list: Vec<String> = fresh.iter().cloned().collect();
        let mut event = Event::new("system.notify", &group.group_id);
        event.by = "system".into();
        event.data = json!({
            "kind": "leftover_batch",
            "priority": "normal",
            "title": "Leftover lane processes",
            "message": format!(
                "{} leftover process batch(es) not owned by a live session: {}. See leftover_processes; opt-in cleanup is runtime.reap_leftover_processes.",
                fresh_list.len(),
                fresh_list.join(", "),
            ),
            "to": ["@foreman"],
            "im_visibility": "internal",
            "context": {
                "triples": fresh_list,
                "count": leftovers.len(),
            },
        })
        .as_object()
        .cloned()
        .unwrap_or_default();
        let Ok(store) = cccc_core::GroupStore::new(home.clone()) else {
            return Ok(out);
        };
        let Ok(path) = store.ledger_path(&group.group_id) else {
            return Ok(out);
        };
        cccc_core::ledger::append(&path, &event)?;
        out.push(event);
    }
    if read_bool(group, "runtime", "reap_leftover_processes") {
        let floor = read_hours(group).saturating_mul(3600);
        for leftover in &leftovers {
            if leftover.age_secs < floor {
                continue;
            }
            // Re-check liveness at reap time: never touch a process that
            // became live, our own tree, pid 1, or anything unattributable
            // (the scan only returns tagged processes, and classify already
            // excluded the live set — this is the belt to that suspenders).
            if leftover.pid <= 1 {
                continue;
            }
            if !is_running(group, &leftover.actor_id)
                || stale_now(home, &leftover.group_id, &leftover.actor_id, leftover.started_secs)
            {
                cccc_runtime::terminate_process(leftover.pid, REAP_GRACE);
            }
        }
    }
    Ok(out)
}

/// Fresh staleness verdict at reap time (the tick's classification may be
/// seconds old; PIDs recycle and sessions restart).
fn stale_now(home: &HomeLayout, group_id: &str, actor_id: &str, started_secs: u64) -> bool {
    let Ok(store) = cccc_core::GroupStore::new(home.clone()) else {
        return false;
    };
    if store.load(group_id).is_err() {
        // Group vanished since classification: still dead, still reapable.
        return true;
    }
    let Ok(path) = store.ledger_path(group_id) else {
        return false;
    };
    let events = cccc_core::ledger::read_all(&path).unwrap_or_default();
    let latest = session_starts(&events)
        .remove(actor_id)
        .unwrap_or(0);
    (started_secs as i64) < latest
}

#[cfg(test)]
mod tests {
    use super::*;
    use cccc_contracts::Actor;

    fn tagged(pid: i32, group_id: &str, actor_id: &str, started_secs: u64) -> TaggedProcess {
        TaggedProcess {
            pid,
            pgid: 4242,
            started_secs,
            comm: "sleep".into(),
            actor_id: actor_id.into(),
            group_id: group_id.into(),
        }
    }

    fn fixture_group(home: &HomeLayout, actor_ids: &[&str]) -> GroupDoc {
        let store = cccc_core::GroupStore::new(home.clone()).expect("store");
        let mut group = store.create("leftover", "").expect("group");
        for actor_id in actor_ids {
            let mut actor = Actor::new(*actor_id);
            actor.runtime = cccc_contracts::ActorRuntime::Opencode;
            group.actors.push(actor);
        }
        store.save(&group).expect("save");
        group
    }

    fn groups_map(home: &HomeLayout) -> HashMap<String, GroupDoc> {
        let store = cccc_core::GroupStore::new(home.clone()).expect("store");
        let mut groups = HashMap::new();
        for group_id in cccc_core::automation::group_ids(home).unwrap_or_default() {
            if let Ok(group) = store.load(&group_id) {
                groups.insert(group_id, group);
            }
        }
        groups
    }

    #[test]
    fn unknown_group_is_group_gone() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("home");
        let groups = groups_map(&home);
        let starts = HashMap::new();
        let process = tagged(42421, "no-such-group", "whoever", 1);
        assert_eq!(
            classify(&process, &groups, &starts, 1, 2),
            Some("group_gone")
        );
    }

    #[test]
    fn unknown_actor_is_actor_gone() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("home");
        let group = fixture_group(&home, &["peer"]);
        let mut groups = HashMap::new();
        groups.insert(group.group_id.clone(), group);
        let gid = groups.keys().next().expect("fixture group").clone();
        let process = tagged(42422, &gid, "ghost", 1);
        assert_eq!(
            classify(&process, &groups, &HashMap::new(), 1, 2),
            Some("actor_gone")
        );
    }

    #[test]
    fn stopped_actor_is_leftover_but_live_self_is_not() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("home");
        let group = fixture_group(&home, &["peer"]);
        let mut groups = HashMap::new();
        groups.insert(group.group_id.clone(), group);
        let gid = groups.keys().next().expect("fixture group").clone();
        // Opencode runtime with no session behind it resolves not-running.
        let stopped = tagged(42423, &gid, "peer", 1);
        assert_eq!(
            classify(&stopped, &groups, &HashMap::new(), 1, 2),
            Some("actor_stopped")
        );
        // Our own pid is never listed, whatever tags it carries.
        let own = tagged(std::process::id() as i32, &gid, "peer", 1);
        assert_eq!(classify(&own, &groups, &HashMap::new(), own.pid, 2), None);
        // Neither is anything else in our own process group.
        let kin = TaggedProcess {
            pgid: 4242,
            ..tagged(42424, &gid, "peer", 1)
        };
        assert_eq!(classify(&kin, &groups, &HashMap::new(), 1, 4242), None);
        // pid 1 is never a lane process.
        let init = tagged(1, &gid, "peer", 1);
        assert_eq!(classify(&init, &groups, &HashMap::new(), 999, 998), None);
    }

    #[test]
    fn process_older_than_latest_session_start_is_stale() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("home");
        let group = fixture_group(&home, &["peer"]);
        let mut groups = HashMap::new();
        groups.insert(group.group_id.clone(), group);
        let gid = groups.keys().next().expect("fixture group").clone();
        let now = now_secs();
        // A session start "now" makes every older process of that actor stale
        // — but staleness only matters for running actors; a stopped actor
        // reports actor_stopped either way.
        let mut starts = HashMap::new();
        starts.insert(gid.clone(), HashMap::from([("peer".to_owned(), now as i64)]));
        let old = tagged(42425, &gid, "peer", now.saturating_sub(3600));
        assert_eq!(
            classify(&old, &groups, &starts, 1, 2),
            Some("actor_stopped")
        );
        // session_starts keeps the NEWEST timestamp per actor, so a late
        // out-of-order event cannot reopen the window.
        let mut first = Event::new("actor.start", &gid);
        first.by = "system".into();
        first.ts = chrono::Utc::now().to_rfc3339();
        first.data = json!({"actor_id": "peer"}).as_object().expect("object").clone();
        let mut events = vec![first];
        let mut older = Event::new("actor.start", &gid);
        older.by = "system".into();
        older.ts = "2020-01-01T00:00:00Z".into();
        older.data = json!({"actor_id": "peer"}).as_object().expect("object").clone();
        events.push(older);
        let map = session_starts(&events);
        assert!(map["peer"] > 1_700_000_000);
    }

    #[test]
    fn notified_batches_are_not_repeated() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("home");
        let group = fixture_group(&home, &["peer"]);
        assert!(notified_triples(&home, &group.group_id).is_empty());
        let store = cccc_core::GroupStore::new(home.clone()).expect("store");
        let path = store.ledger_path(&group.group_id).expect("ledger");
        let mut event = Event::new("system.notify", &group.group_id);
        event.by = "system".into();
        event.data = json!({
            "kind": "leftover_batch",
            "to": ["@foreman"],
            "context": {"triples": ["g:a:actor_stopped"]},
        })
        .as_object()
        .expect("object")
        .clone();
        cccc_core::ledger::append(&path, &event).expect("append");
        assert!(notified_triples(&home, &group.group_id).contains("g:a:actor_stopped"));
    }

    #[test]
    fn reap_settings_default_off_with_24h_floor() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("home");
        let group = fixture_group(&home, &["peer"]);
        assert!(!read_bool(&group, "runtime", "reap_leftover_processes"));
        assert_eq!(read_hours(&group), 24);
    }
}
