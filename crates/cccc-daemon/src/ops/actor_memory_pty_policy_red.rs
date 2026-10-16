//! Baseline-compatible functional regressions: only APIs present at 4f93.
//! No provider traffic, global environment mutation, or simulated cached RSS.
use super::*;
use crate::ops::{actor_delivery, actor_runtime};
use cccc_contracts::{Actor, ActorRuntime, GroupState};
use cccc_core::{GroupStore, group_scope, scope};
use std::time::{Duration, Instant};

pub(super) struct Fixture {
    _temp: tempfile::TempDir,
    pub(super) home: HomeLayout,
    pub(super) group: GroupDoc,
}
impl Fixture {
    pub(super) fn new(disabled_foreman: bool) -> Self {
        let temp = tempfile::tempdir().expect("private fixture");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("initialize");
        let store = GroupStore::new(home.clone()).expect("store");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let created = store.create("PTY memory detection", "").expect("group");
        let mut group = group_scope::attach(
            &store,
            &created.group_id,
            scope::detect(&workspace).expect("scope"),
        )
        .expect("attach");
        let mut lead = Actor::new("lead");
        lead.memory_limit_mb = 0;
        lead.enabled = !disabled_foreman;
        let mut worker = Actor::new("worker");
        worker.runtime = ActorRuntime::Custom;
        worker.command = vec!["python3".into(), "-u".into(), "-c".into(),
            "import sys; keep=bytearray(8*1024*1024); print('memory-fixture-ready',flush=True); [None for line in sys.stdin]".into()];
        worker.memory_limit_mb = 1;
        worker.env.insert(
            "HOME".into(),
            temp.path().join("provider").to_string_lossy().into_owned(),
        );
        worker.normalize_runtime_constraints();
        group.actors = vec![lead, worker];
        group.running = true;
        group.state = GroupState::Active;
        store.save(&group).expect("save");
        let f = Self {
            _temp: temp,
            home,
            group,
        };
        actor_runtime::apply(&f.home, &f.group, "worker", "actor.start").expect("owned PTY");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if cccc_runtime::activity(&f.group.group_id, "worker")
                .is_ok_and(|s| s.last_output.is_some())
            {
                break;
            }
            assert!(Instant::now() < deadline, "offline PTY never became ready");
            std::thread::sleep(Duration::from_millis(10));
        }
        f
    }
    pub(super) fn status(&self) -> cccc_runtime::SessionStatus {
        cccc_runtime::status(&self.group.group_id, "worker").expect("live owned PTY")
    }
    fn ledger_notices(&self) -> Vec<Event> {
        let store = GroupStore::new(self.home.clone()).expect("store");
        cccc_core::ledger::read_all(&store.ledger_path(&self.group.group_id).expect("ledger"))
            .expect("read ledger")
            .into_iter()
            .filter(|e| e.kind == "system.notify" && e.data["kind"] == "memory_restart")
            .collect()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        actor_delivery::shutdown_group(&self.group.group_id);
        let _ = actor_runtime::apply(&self.home, &self.group, "worker", "actor.stop");
    }
}

#[test]
fn explicit_pty_opt_in_round_trips_and_default_stays_off() {
    let plain = serde_json::to_value(Actor::new("worker")).expect("serialize");
    assert!(plain.get("memory_limit_restart_pty").is_none());
    assert_eq!(plain["memory_limit_mb"], 4096);
    let mut configured = plain;
    configured["memory_limit_restart_pty"] = json!(true);
    let actor: Actor = serde_json::from_value(configured).expect("configured actor");
    // Baseline ignores this unknown field, so this is assertion RED, not compile RED.
    assert_eq!(
        serde_json::to_value(actor).expect("serialize")["memory_limit_restart_pty"],
        true
    );
}

fn assert_detection(disabled_foreman: bool) {
    let f = Fixture::new(disabled_foreman);
    let before = f.status();
    let events = tick(&f.home, &f.group).expect("actual scheduler memory tick");
    assert_eq!(
        events.len(),
        1,
        "non-opt-in over-limit PTY must report without restart"
    );
    let event = &events[0];
    assert_eq!(event.data["target_actor_id"], "lead");
    assert_eq!(event.data["to"], json!(["@foreman"]));
    assert_eq!(event.data["context"]["action"], "detection_only");
    assert_eq!(event.data["context"]["outcome"], "over_limit");
    assert!(
        event.data["context"]["memory_mb"]
            .as_u64()
            .expect("real sample")
            > 1
    );
    let after = f.status();
    assert_eq!(before.pid, after.pid, "notice must not replace the process");
    assert_eq!(before.started_at, after.started_at);
    assert!(
        !cooldown_path(&f.home, &f.group.group_id)
            .expect("restart cooldown")
            .exists(),
        "held detection must not reserve restart cooldown"
    );
    assert!(
        tick(&f.home, &f.group).expect("second tick").is_empty(),
        "dedupe applies to detection"
    );
    assert_eq!(
        f.ledger_notices().len(),
        1,
        "exactly one durable targeted record"
    );
    let store = GroupStore::new(f.home.clone()).expect("store");
    let reread = store.load(&f.group.group_id).expect("reopen group");
    assert!(
        tick(&f.home, &reread)
            .expect("reopened cooldown")
            .is_empty()
    );
    assert_eq!(f.ledger_notices().len(), 1);
    // The foreman has never been started; delivery is durable rather than a wake.
    assert!(!cccc_runtime::status(&f.group.group_id, "lead").is_ok_and(|s| s.running));
    assert!(
        crate::ops::runtime_delivery::pending_sources(&f.home, &f.group, &f.group.actors[1], 10)
            .expect("worker inbox")
            .is_empty(),
        "never broadcast to the worker"
    );
}

#[test]
fn actual_over_limit_non_opt_in_pty_targets_down_foreman_once_without_restart() {
    assert_detection(false);
}
#[test]
fn actual_over_limit_non_opt_in_pty_targets_disabled_foreman_without_waking_it() {
    assert_detection(true);
}

#[test]
fn explicit_zero_or_disabled_actor_has_no_detection_or_restart_side_effect() {
    for disabled in [false, true] {
        let mut f = Fixture::new(false);
        if disabled {
            f.group.actors[1].enabled = false;
        } else {
            f.group.actors[1].memory_limit_mb = 0;
        }
        GroupStore::new(f.home.clone())
            .expect("store")
            .save(&f.group)
            .expect("save control");
        let before = f.status();
        assert!(tick(&f.home, &f.group).expect("off tick").is_empty());
        assert!(f.ledger_notices().is_empty());
        assert_eq!(f.status().pid, before.pid);
        assert!(
            !cooldown_path(&f.home, &f.group.group_id)
                .expect("path")
                .exists()
        );
    }
}

#[test]
fn no_configured_visible_foreman_is_diagnostic_and_does_not_consume_notice_cooldown() {
    let mut f = Fixture::new(false);
    // Internal actors never acquire the effective foreman role.
    for actor in &mut f.group.actors {
        actor.internal_kind = Some("fixture_hidden".into());
    }
    let store = GroupStore::new(f.home.clone()).expect("store");
    store.save(&f.group).expect("save no foreman");
    let before = f.status();
    assert!(
        tick(&f.home, &f.group).is_err(),
        "unroutable detection must be surfaced"
    );
    assert!(
        f.ledger_notices().is_empty(),
        "no targetless or broadcast notice"
    );
    assert!(
        !store
            .state_dir(&f.group.group_id)
            .expect("state")
            .join("actor_memory_detection.json")
            .exists()
    );
    assert_eq!(f.status().pid, before.pid);
    f.group.actors[0].internal_kind = None;
    store.save(&f.group).expect("restore configured foreman");
    let events = tick(&f.home, &f.group).expect("retry delivery without burned cooldown");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data["target_actor_id"], "lead");
}
