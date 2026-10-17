use super::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::time::SystemTime;

// Explicit FIX-only clock seam, keyed to a private test group/actor and thread.
// Production quiet_live reads the unmodified monotonic runtime activity snapshot.
thread_local! { static AGE:RefCell<HashMap<(String,String),Duration>>=RefCell::new(HashMap::new()); }
pub(super) fn aged_for_test(
    group: &str,
    actor: &str,
    mut snapshot: ActivitySnapshot,
) -> ActivitySnapshot {
    AGE.with(|ages| {
        if let Some(age) = ages.borrow().get(&(group.into(), actor.into())) {
            if let Some(output) = &mut snapshot.last_output {
                output.age = output.age.saturating_add(*age);
            }
            if let Some(input) = &mut snapshot.last_input {
                input.age = input.age.saturating_add(*age);
            }
        }
    });
    snapshot
}
struct Age(String, String);
impl Age {
    fn new(group: &str, actor: &str) -> Self {
        AGE.with(|ages| {
            assert!(
                ages.borrow_mut()
                    .insert((group.into(), actor.into()), Duration::from_secs(600))
                    .is_none()
            )
        });
        Self(group.into(), actor.into())
    }
}
impl Drop for Age {
    fn drop(&mut self) {
        AGE.with(|ages| {
            ages.borrow_mut().remove(&(self.0.clone(), self.1.clone()));
        });
    }
}

fn snapshot(output: Option<u64>, input: Option<u64>) -> ActivitySnapshot {
    let instant = |age| cccc_runtime::ActivityInstant {
        at: SystemTime::now(),
        age: Duration::from_secs(age),
    };
    ActivitySnapshot {
        last_output: output.map(instant),
        last_input: input.map(instant),
        recent_output: Vec::new(),
        output_bytes_since_input: 0,
    }
}
#[test]
fn literal_cf959_quiet_boundaries_require_observed_output_and_no_newer_input() {
    let quiet = Duration::from_secs(600);
    for (output, input, expected) in [
        (Some(1200), Some(1300), true),
        (Some(1200), None, true),
        (Some(599), Some(3600), false),
        (Some(600), Some(600), true),
        (Some(1200), Some(60), false),
        (None, None, false),
        (None, Some(5000), false),
    ] {
        assert_eq!(pty_quiet_for(&snapshot(output, input), quiet), expected);
    }
    assert!(!quiet_live("missing-private-test-group", "missing-actor"));
}

#[test]
fn all_non_native_runtime_modes_use_pty_policy_even_with_managed_state_source() {
    for runtime in [
        cccc_contracts::ActorRuntime::Codex,
        cccc_contracts::ActorRuntime::Claude,
        cccc_contracts::ActorRuntime::Kilo,
        cccc_contracts::ActorRuntime::Opencode,
        cccc_contracts::ActorRuntime::Grok,
        cccc_contracts::ActorRuntime::Pi,
        cccc_contracts::ActorRuntime::Deepseek,
        cccc_contracts::ActorRuntime::Custom,
    ] {
        let mut actor = Actor::new("worker");
        actor.runtime = runtime;
        actor.normalize_runtime_constraints();
        assert!(!is_native_acp(&actor), "{runtime:?}");
    }
    let mut native = Actor::new("worker");
    native.runtime = cccc_contracts::ActorRuntime::Hermes;
    native.runtime_mode = cccc_contracts::RuntimeMode::Acp;
    native.normalize_runtime_constraints();
    assert!(is_native_acp(&native));
}

fn opted_in_fixture() -> super::super::pty_policy_red::Fixture {
    let mut f = super::super::pty_policy_red::Fixture::new(false);
    f.group.actors[1].memory_limit_restart_pty = true;
    cccc_core::GroupStore::new(f.home.clone())
        .expect("store")
        .save(&f.group)
        .expect("save explicit opt-in");
    f
}

#[test]
fn real_owned_pty_fresh_restart_replaces_pid_discards_saved_receipt_and_labels_no_resume() {
    let f = opted_in_fixture();
    let old = f.status();
    let saved =
        json!({"provider_thread_id":"old-session-must-not-be-resumed","sentinel":"old-receipt"});
    super::super::super::runtime_session::restore_snapshot(
        &f.home,
        &f.group.group_id,
        "worker",
        Some(saved.as_object().expect("receipt")),
    )
    .expect("saved receipt");
    let _age = Age::new(&f.group.group_id, "worker");
    let events = super::super::tick(&f.home, &f.group).expect("actual fresh helper");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data["context"]["action"], "pty_fresh_restart");
    assert_eq!(events[0].data["context"]["outcome"], "restarted");
    assert!(
        events[0].data["message"]
            .as_str()
            .expect("message")
            .contains("fresh restart (no resume)")
    );
    assert_ne!(
        f.status().pid,
        old.pid,
        "owned process was actually replaced"
    );
    assert!(
        super::super::super::runtime_session::snapshot(&f.home, &f.group.group_id, "worker")
            .expect("receipt")
            .is_none(),
        "actor.restart would have retained this receipt"
    );
    assert!(
        super::super::cooldown_path(&f.home, &f.group.group_id)
            .expect("attempt path")
            .exists()
    );
    assert!(
        !super::super::required_resume_path(&f.home, &f.group.group_id, "worker")
            .expect("fence path")
            .exists()
    );
}

#[test]
fn pty_opt_in_never_consumes_existing_native_resume_obligation_even_when_quiet() {
    let f = opted_in_fixture();
    let before = f.status();
    let required = super::super::RequiredResume {
        generation: "old-native-generation".into(),
        runtime: cccc_contracts::ActorRuntime::Hermes,
        session_id: "old-native-session".into(),
    };
    super::super::require_resume(&f.home, &f.group.group_id, "worker", &required)
        .expect("required native resume");
    let path =
        super::super::required_resume_path(&f.home, &f.group.group_id, "worker").expect("path");
    let bytes = std::fs::read(&path).expect("exact bytes");
    let _age = Age::new(&f.group.group_id, "worker");
    let events = super::super::tick(&f.home, &f.group).expect("held tick");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data["context"]["action"], "detection_only");
    assert_eq!(before.pid, f.status().pid);
    assert_eq!(std::fs::read(&path).expect("retained exact fence"), bytes);
    assert!(
        !super::super::cooldown_path(&f.home, &f.group.group_id)
            .expect("path")
            .exists()
    );
}

#[test]
fn corrupt_native_obligation_reports_held_detection_instead_of_becoming_fresh_permission() {
    let f = opted_in_fixture();
    let before = f.status();
    let path =
        super::super::required_resume_path(&f.home, &f.group.group_id, "worker").expect("path");
    cccc_core::fs::atomic_write(&path, b"{corrupt").expect("corrupt fence");
    let _age = Age::new(&f.group.group_id, "worker");
    let events = super::super::tick(&f.home, &f.group).expect("diagnostic notice");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data["context"]["action"], "detection_only");
    assert_eq!(std::fs::read(&path).expect("unchanged"), b"{corrupt");
    assert_eq!(f.status().pid, before.pid);
    assert!(
        !super::super::cooldown_path(&f.home, &f.group.group_id)
            .expect("path")
            .exists()
    );
}

#[test]
fn detection_pending_append_recovery_never_duplicates_or_uses_attempt_marker() {
    let f = super::super::pty_policy_red::Fixture::new(false);
    let store = cccc_core::GroupStore::new(f.home.clone()).expect("store");
    let path = store
        .state_dir(&f.group.group_id)
        .expect("state")
        .join(DETECTION_FILE);
    let mut event = Event::new("system.notify", &f.group.group_id);
    event.by = "system".into();
    event.data =
        json!({"kind":"memory_restart","target_actor_id":"lead","context":{"actor_id":"worker"}})
            .as_object()
            .expect("data")
            .clone();
    cccc_core::ledger::append(
        &store.ledger_path(&f.group.group_id).expect("ledger"),
        &event,
    )
    .expect("append committed before crash");
    let marks = HashMap::from([(
        "worker".to_owned(),
        DetectionMark {
            at: now_secs(),
            pending: Some(event.clone()),
        },
    )]);
    cccc_core::fs::write_json(&path, &marks)
        .expect("pending marker simulates crash before commit mark");
    assert!(
        emit_detection(&f.home, &f.group, event.clone())
            .expect("recover pending")
            .is_none()
    );
    let events =
        cccc_core::ledger::read_all(&store.ledger_path(&f.group.group_id).expect("ledger"))
            .expect("read");
    assert_eq!(events.iter().filter(|e| e.id == event.id).count(), 1);
    assert!(
        !super::super::cooldown_path(&f.home, &f.group.group_id)
            .expect("attempt marker")
            .exists()
    );
}

#[test]
fn failed_attempt_persistence_leaves_owned_process_and_saved_receipt_untouched() {
    let f = opted_in_fixture();
    let before = f.status();
    let saved = json!({"provider_thread_id":"retained-on-no-attempt","sentinel":"saved"});
    super::super::super::runtime_session::restore_snapshot(
        &f.home,
        &f.group.group_id,
        "worker",
        Some(saved.as_object().expect("receipt")),
    )
    .expect("saved receipt");
    let _age = Age::new(&f.group.group_id, "worker");
    let sample = super::super::observation::observe(&f.home, &f.group, &f.group.actors[1])
        .expect("actual observation")
        .expect("owned root");
    let error = super::super::super::actor_runtime::restart_for_memory_pty(
        &f.home,
        &f.group.group_id,
        "worker",
        &sample,
        |_, _| Ok(true),
        || Err(io::Error::other("injected persistence failure")),
    )
    .expect_err("no destructive attempt without durable reservation");
    assert_eq!(error.to_string(), "injected persistence failure");
    assert_eq!(f.status().pid, before.pid);
    assert!(f.status().running);
    assert_eq!(
        super::super::super::runtime_session::snapshot(&f.home, &f.group.group_id, "worker")
            .expect("retained receipt"),
        Some(saved.as_object().expect("receipt").clone())
    );
    assert!(
        !super::super::cooldown_path(&f.home, &f.group.group_id)
            .expect("path")
            .exists()
    );
}

#[test]
fn pending_notice_recovers_even_after_actor_limit_was_disabled() {
    let mut f = super::super::pty_policy_red::Fixture::new(false);
    let store = cccc_core::GroupStore::new(f.home.clone()).expect("store");
    let path = store
        .state_dir(&f.group.group_id)
        .expect("state")
        .join(DETECTION_FILE);
    let before = f.status();
    let mut event = Event::new("system.notify", &f.group.group_id);
    event.by = "system".into();
    event.data=json!({"kind":"memory_restart","to":["@foreman"],"target_actor_id":"lead","context":{"actor_id":"worker","action":"detection_only","outcome":"over_limit"}}).as_object().expect("data").clone();
    cccc_core::fs::write_json(
        &path,
        &HashMap::from([(
            "worker".to_owned(),
            DetectionMark {
                at: now_secs(),
                pending: Some(event.clone()),
            },
        )]),
    )
    .expect("crash before append");
    f.group.actors[1].memory_limit_mb = 0;
    store
        .save(&f.group)
        .expect("disable sampling after pending observation");
    let events = super::super::tick(&f.home, &f.group).expect("drain previously observed notice");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id, event.id);
    let pending = super::super::super::runtime_delivery::pending_sources(
        &f.home,
        &f.group,
        &f.group.actors[0],
        10,
    )
    .expect("durable foreman inbox");
    assert_eq!(pending.iter().filter(|e| e.id == event.id).count(), 1);
    assert_eq!(f.status().pid, before.pid);
    assert!(
        !super::super::cooldown_path(&f.home, &f.group.group_id)
            .expect("path")
            .exists()
    );
    assert!(
        super::super::tick(&f.home, &f.group)
            .expect("second drain")
            .is_empty()
    );
}

#[test]
fn real_fresh_attempt_cooldown_suppresses_later_unreadable_profile_detection() {
    let mut f = opted_in_fixture();
    let store = cccc_core::GroupStore::new(f.home.clone()).expect("store");
    let ledger_path = store.ledger_path(&f.group.group_id).expect("ledger path");
    let attempt_path =
        super::super::cooldown_path(&f.home, &f.group.group_id).expect("actual attempt path");
    let detection_path = store
        .state_dir(&f.group.group_id)
        .expect("state directory")
        .join(DETECTION_FILE);
    let original_pid = f.status().pid;
    let _age = Age::new(&f.group.group_id, "worker");
    let first = super::super::tick(&f.home, &f.group).expect("actual fresh restart");
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].data["context"]["action"], "pty_fresh_restart");
    assert_eq!(first[0].data["context"]["outcome"], "restarted");
    let replacement_pid = f.status().pid;
    assert_ne!(
        original_pid, replacement_pid,
        "a real owned restart occurred"
    );
    let attempt_before = std::fs::read(&attempt_path).expect("durable actual attempt");
    let ledger_before = cccc_core::ledger::read_all(&ledger_path).expect("ledger before");
    assert!(
        !detection_path.exists(),
        "the actual attempt is not a detection mark"
    );

    // The owned replacement stays alive. Only its next effective profile becomes
    // unreadable; resolving a missing linked profile uses the real error branch.
    f.group.actors[1].profile_id = "missing-private-memory-profile".into();
    store
        .save(&f.group)
        .expect("persist missing linked profile");
    let later = super::super::tick(&f.home, &f.group).expect("cooldown tick");
    let ledger_after = cccc_core::ledger::read_all(&ledger_path).expect("ledger after");
    let memory_notices = |events: &[Event]| {
        events
            .iter()
            .filter(|event| event.kind == "system.notify" && event.data["kind"] == "memory_restart")
            .map(|event| event.id.clone())
            .collect::<Vec<_>>()
    };
    assert!(
        later.is_empty(),
        "a real attempt already reported this window"
    );
    assert_eq!(
        memory_notices(&ledger_after),
        memory_notices(&ledger_before)
    );
    assert_eq!(
        std::fs::read(&attempt_path).expect("unchanged attempt"),
        attempt_before
    );
    assert!(
        !detection_path.exists(),
        "no extra detection cooldown is consumed"
    );
    assert_eq!(f.status().pid, replacement_pid);
    assert!(f.status().running);
}

#[test]
fn already_appended_pending_detection_is_offered_once_when_sampling_is_disabled() {
    let mut f = super::super::pty_policy_red::Fixture::new(false);
    let store = cccc_core::GroupStore::new(f.home.clone()).expect("store");
    let ledger_path = store.ledger_path(&f.group.group_id).expect("ledger");
    let detection_path = store
        .state_dir(&f.group.group_id)
        .expect("state")
        .join(DETECTION_FILE);
    let before_pid = f.status().pid;
    let mut event = Event::new("system.notify", &f.group.group_id);
    event.by = "system".into();
    event.data = json!({
        "kind":"memory_restart", "to":["@foreman"], "target_actor_id":"lead",
        "context":{"actor_id":"worker", "action":"detection_only", "outcome":"over_limit"}
    })
    .as_object()
    .expect("event data")
    .clone();
    cccc_core::ledger::append(&ledger_path, &event).expect("append before interrupted commit");
    cccc_core::fs::write_json(
        &detection_path,
        &HashMap::from([(
            "worker".to_owned(),
            DetectionMark {
                at: now_secs(),
                pending: Some(event.clone()),
            },
        )]),
    )
    .expect("pending event whose append already committed");
    f.group.actors[1].memory_limit_mb = 0;
    store.save(&f.group).expect("disable subsequent sampling");

    let recovered = super::super::tick(&f.home, &f.group).expect("actual recovery pass");
    let second = super::super::tick(&f.home, &f.group).expect("second recovery pass");
    let ledger = cccc_core::ledger::read_all(&ledger_path).expect("ledger after recovery");
    let marks: HashMap<String, DetectionMark> =
        serde_json::from_slice(&std::fs::read(&detection_path).expect("committed marker"))
            .expect("detection marks");
    assert_eq!(
        recovered.len(),
        1,
        "offer the existing event to the dispatch caller"
    );
    assert_eq!(
        recovered[0].id, event.id,
        "never allocate another event identity"
    );
    assert_eq!(recovered[0].data, event.data);
    assert!(
        second.is_empty(),
        "one recovery offer after clearing the marker"
    );
    assert_eq!(
        ledger.iter().filter(|record| record.id == event.id).count(),
        1
    );
    assert!(marks.get("worker").expect("actor mark").pending.is_none());
    assert_eq!(f.status().pid, before_pid);
    assert!(
        !super::super::cooldown_path(&f.home, &f.group.group_id)
            .expect("attempt path")
            .exists()
    );
}

// ---- MUST3 (tests-only): input record vs the quiet-to-stop gap ----
// Real interleavings only: every submit below travels the actor's own
// entrypoint; no activity field is ever written by a test.

/// The regression itself: a REAL accepted submit lands in the final
/// quiet-to-stop gap of the maintenance flow. The owned actor must survive
/// (same pid, still running) and the attempt cooldown must stay unspent —
/// the stop decision has to recheck under the actor guard, not trust the
/// earlier quiet observation. RED on current source: nothing rechecks, so
/// the flow commits (cooldown written) and stops the actor anyway.
#[test]
fn memory_must3_real_submit_in_the_final_quiet_to_stop_gap_keeps_the_owned_actor_and_spends_no_cooldown()
 {
    let f = opted_in_fixture();
    let before = f.status();
    let _age = Age::new(&f.group.group_id, "worker");
    let sample = super::super::observation::observe(&f.home, &f.group, &f.group.actors[1])
        .expect("actual observation")
        .expect("owned root");
    let _arm = crate::ops::actor_memory::must3_gap_tests::arm(
        f.home.root().to_path_buf(),
        f.group.group_id.clone(),
        "worker".into(),
    );
    // Two DISTINCT oracles, per MUST3 review: (a) the commit-call counter
    // proves the attempt boundary was never reached at all; (b) if commit
    // WERE invoked, it performs the REAL cooldown write (same owner/actor and
    // now-secs semantics the coordinator uses), so cooldown-file absence can
    // never be vacuous.
    let commit_calls = std::sync::atomic::AtomicUsize::new(0);
    let outcome = super::super::super::actor_runtime::restart_for_memory_pty(
        &f.home,
        &f.group.group_id,
        "worker",
        &sample,
        |_, _| Ok(true),
        || {
            commit_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            super::super::write_cooldown(
                &f.home,
                &f.group.group_id,
                "worker",
                super::super::now_secs(),
            )
        },
    )
    .expect("maintenance returns");
    assert_eq!(
        outcome,
        super::super::super::actor_runtime::FreshOutcome::Skipped,
        "newer input in the gap must veto the stop"
    );
    assert!(f.status().running, "owned actor must stay alive");
    assert_eq!(
        f.status().pid,
        before.pid,
        "the actor must be the same owned process, not a replacement"
    );
    assert!(
        commit_calls.load(std::sync::atomic::Ordering::SeqCst) == 0,
        "the attempt boundary must never be reached when the stop is vetoed \
         (this counter is the commit-reached oracle, distinct from the \
         persisted-cooldown oracle below)"
    );
    assert!(
        !super::super::cooldown_path(&f.home, &f.group.group_id)
            .expect("attempt path")
            .exists(),
        "no attempt may be persisted when the stop is vetoed - and if commit \
         had run, it wrote the REAL cooldown, so this file's absence is proof"
    );
}

/// Control: a submit cancelled before admission records NO input — a
/// cancelled-before-accept send must not veto a later legitimate stop.
#[test]
fn memory_must3_cancelled_before_admission_records_no_input() {
    let f = opted_in_fixture();
    let cancelled = std::sync::atomic::AtomicBool::new(true);
    let accepted = super::super::super::actor_delivery::submit_terminal_text(
        &f.group.group_id,
        &f.group.actors[1],
        "must3 cancelled control",
        &cancelled,
    );
    assert!(!accepted, "a cancelled submit must not deliver");
    let snapshot = cccc_runtime::activity(&f.group.group_id, "worker").expect("activity");
    assert!(
        snapshot.last_input.is_none(),
        "cancelled-before-accept must leave the input record untouched"
    );
}

/// Pin: a real ACCEPTED submit records input before it returns, which is the
/// record the stop-side recheck reads. GREEN today (terminal side): this is
/// the contract the gap regression protects, stated independently of it.
#[test]
fn memory_must3_accepted_submit_records_input_before_it_returns() {
    let f = opted_in_fixture();
    let accepted = super::super::super::actor_delivery::submit_terminal_text(
        &f.group.group_id,
        &f.group.actors[1],
        "must3 accepted pin",
        &std::sync::atomic::AtomicBool::new(false),
    );
    assert!(accepted, "the fixture submit must be accepted");
    let snapshot = cccc_runtime::activity(&f.group.group_id, "worker").expect("activity");
    assert!(
        snapshot.last_input.is_some(),
        "an accepted submit must record input before completion"
    );
}
