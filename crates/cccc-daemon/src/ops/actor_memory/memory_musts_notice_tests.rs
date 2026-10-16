//! Tests-only proposals v4: notice state, reachable ghost reasons (corrects v3).
//!
//! v1 (`../tests-v1/`), v2 (`../tests-v2/`) and v3 (`../tests-v3/`) are
//! PRESERVED IMMUTABLY. This file fixes v3 finding: the v2/v3 Deepseek ghost
//! derivation was FALSE — `actor_runtime_status::resolve` short-circuits
//! Deepseek (and managed runtimes) to not-running BEFORE the structured
//! fallback, so identity yields Ok(None) and setup emits zero. The ghost here
//! is WebModel (derivation in `ghost_fixture`); per-actor single-ID A→B→A
//! shape is otherwise unchanged: worker disabled throughout, ghost profile
//! empty → MISSING → empty, every event asserts `context.actor_id == "ghost"`.
//! Baseline 4881 therefore fails directly at B (suppressed), not on a return
//! to a different actor.
//!
//! Ghost reason derivation (each link verified at immutable 4881 —
//! including the early returns v3 missed): a NEVER-STARTED WebModel actor
//! yields "Running runtime has no attributable per-actor…". resolve() clones
//! on empty profile_id; `normalize` maps WebModel→Headless via plain
//! `runner()` in every mode (WebModel ∉ `supports_acp_mode` and ∉ the managed
//! list, so `uses_managed_session` stays false and `is_structured` holds);
//! `identity()` finds no managed entry, `cccc_runtime::status` fails
//! (never started), and `actor_runtime_status::resolve` — past the Deepseek/
//! supports early return, past the dead session branch — reports running
//! (enabled && group running && Active) with pid None. If any link rots,
//! setup asserts fail LOUD at tick 1, never silently.
//!
//! Wiring: same as v1/v2 (pty.rs `#[path] mod`, linux-gated).
//! RED/GREEN vs 4881: `_red_on_4881` MUST FAIL on 4881; `_preserved` MUST
//! PASS on 4881 and after. Mark-file time fixtures only (`age_mark_class`
//! with explicit class clocks, single-borrow form); real `tick_detection`
//! discovery throughout; no manufactured outcomes.

use super::*;
use cccc_contracts::{Actor, ActorRuntime};
use cccc_core::GroupStore;

const MISSING_PROFILE: &str = "missing-profile-for-memory-tests";
const WINDOW_AGO_SECS: u64 = 3600;
const REASON_PROFILE: &str = "Current runtime profile could not be resolved; no automatic restart.";
const REASON_UNATTRIBUTABLE: &str = "Running runtime has no attributable per-actor process root; external provider memory is unmeasured";

fn detection_marks_path(home: &HomeLayout, group_id: &str) -> std::path::PathBuf {
    GroupStore::new(home.clone())
        .expect("store")
        .state_dir(group_id)
        .expect("state dir")
        .join("actor_memory_detection.json")
}

fn age_mark_class(home: &HomeLayout, group_id: &str, actor: &str, class: &str, secs_ago: u64) {
    let path = detection_marks_path(home, group_id);
    let mut doc: serde_json::Value = cccc_core::fs::read_json(&path).expect("marks file");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    let nested = doc.get(actor).and_then(|entry| entry.get(class)).is_some();
    let target = if nested {
        doc.get_mut(actor)
            .and_then(|entry| entry.get_mut(class))
            .expect("nested class mark")
    } else {
        doc.get_mut(actor).expect("actor mark")
    };
    target["at"] = serde_json::json!(now.saturating_sub(secs_ago));
    cccc_core::fs::write_json_committed(&path, &doc).expect("age marks");
}

fn memory_notices(home: &HomeLayout, group_id: &str) -> Vec<serde_json::Value> {
    let store = GroupStore::new(home.clone()).expect("store");
    let ledger = store.ledger_path(group_id).expect("ledger");
    cccc_core::ledger::read_all(&ledger)
        .expect("read ledger")
        .into_iter()
        .filter(|e| e.kind == "system.notify" && e.data["kind"] == "memory_restart")
        .map(|e| serde_json::to_value(e).expect("event json"))
        .collect()
}

fn actor_mut<'g>(group: &'g mut GroupDoc, actor_id: &str) -> &'g mut Actor {
    group
        .actors
        .iter_mut()
        .find(|a| a.id == actor_id)
        .expect("actor")
}

fn outcome_of(event: &serde_json::Value) -> &str {
    event["data"]["context"]["outcome"].as_str().unwrap_or("")
}

fn message_of(event: &serde_json::Value) -> &str {
    event["data"]["message"].as_str().unwrap_or("")
}

fn event_actor_of(event: &serde_json::Value) -> &str {
    event["data"]["context"]["actor_id"].as_str().unwrap_or("")
}

/// ONE enabled actor (ghost) for the whole A→B→A test; worker disabled
/// throughout so no second actor ID can satisfy the assertions.
fn ghost_fixture() -> (super::super::pty_policy_red::Fixture, GroupDoc) {
    let f = super::super::pty_policy_red::Fixture::new(false);
    let mut group = GroupDoc::clone(&f.group);
    for actor in group.actors.iter_mut() {
        actor.enabled = false;
    }
    let mut ghost = Actor::new("ghost");
    // WebModel — DELIBERATELY not Deepseek. resolve()
    // (actor_runtime_status.rs) short-circuits `runtime == Deepseek ||
    // supports(actor)` to not-running BEFORE the structured fallback, so a
    // never-started Deepseek ghost yields identity Ok(None) — silent, and the
    // v2/v3 reason derivation was false on that exact line. WebModel maps to
    // a Headless runner via plain `runner()` in every mode (never via
    // headless-ACP, so `uses_managed_session` stays false and `is_structured`
    // holds); normalize only refreshes runner/state-source (and clears the
    // never-used command). A never-started, enabled WebModel ghost in a
    // running Active group therefore reports running with pid None, and
    // identity() takes the attributable-root Err arm. Verified link by link
    // at 4881; setup asserts fail loud at tick 1 if any link rots.
    ghost.runtime = ActorRuntime::WebModel;
    ghost.memory_limit_mb = 1;
    ghost.enabled = true;
    group.actors.push(ghost);
    (f, group)
}

fn worker_fixture() -> (super::super::pty_policy_red::Fixture, GroupDoc) {
    let f = super::super::pty_policy_red::Fixture::new(false);
    // Clone into a local FIRST: `(f, GroupDoc::clone(&f.group))` moves `f`
    // before borrowing it (E0382). (ghost_fixture already had the right order.)
    let group = GroupDoc::clone(&f.group);
    (f, group)
}

#[test]
fn same_actor_distinct_reasons_a_b_a_red_on_4881() {
    let (f, mut group) = ghost_fixture();
    let tick_a1 = super::tick_detection(&f.home, &group).expect("tick A1");
    assert_eq!(
        tick_a1.len(),
        1,
        "ghost unattributable notifies (reason A setup)"
    );
    let notices = memory_notices(&f.home, &group.group_id);
    assert_eq!(event_actor_of(&notices[0]), "ghost");
    assert_eq!(outcome_of(&notices[0]), "detection_unavailable");
    assert!(
        message_of(&notices[0]).contains(REASON_UNATTRIBUTABLE),
        "reason A, got: {}",
        message_of(&notices[0])
    );
    actor_mut(&mut group, "ghost").profile_id = MISSING_PROFILE.into();
    let tick_b = super::tick_detection(&f.home, &group).expect("tick B");
    assert_eq!(
        tick_b.len(),
        1,
        "same-actor reason change A->B notifies within window"
    );
    let notices = memory_notices(&f.home, &group.group_id);
    assert_eq!(event_actor_of(&notices[1]), "ghost");
    assert!(
        message_of(&notices[1]).contains(REASON_PROFILE),
        "reason B, got: {}",
        message_of(&notices[1])
    );
    actor_mut(&mut group, "ghost").profile_id = String::new();
    let tick_a2 = super::tick_detection(&f.home, &group).expect("tick A2");
    assert_eq!(
        tick_a2.len(),
        1,
        "same-actor reason change B->A notifies within window"
    );
    let notices = memory_notices(&f.home, &group.group_id);
    assert_eq!(notices.len(), 3);
    assert!(notices.iter().all(|e| event_actor_of(e) == "ghost"));
    assert!(
        notices
            .iter()
            .all(|e| outcome_of(e) == "detection_unavailable")
    );
    assert!(message_of(&notices[2]).contains(REASON_UNATTRIBUTABLE));
}

#[test]
fn unavailable_repeats_silently_after_window_expiry_red_on_4881() {
    let (f, group) = ghost_fixture();
    let first = super::tick_detection(&f.home, &group).expect("tick 1");
    assert_eq!(first.len(), 1, "first unavailable occurrence notifies");
    age_mark_class(
        &f.home,
        &group.group_id,
        "ghost",
        "unavailable",
        WINDOW_AGO_SECS,
    );
    let second = super::tick_detection(&f.home, &group).expect("tick 2");
    assert!(
        second.is_empty(),
        "unchanged state must not re-notify after window expiry"
    );
    assert_eq!(memory_notices(&f.home, &group.group_id).len(), 1);
}

#[test]
fn limit_change_with_same_reason_stays_silent_preserved() {
    // Rendered message embeds the limit, but the key is (outcome, static
    // detail): green on 4881 (window) AND fixed (same state).
    let (f, mut group) = ghost_fixture();
    let first = super::tick_detection(&f.home, &group).expect("tick 1");
    assert_eq!(first.len(), 1);
    let before = memory_notices(&f.home, &group.group_id);
    assert!(message_of(&before[0]).contains("limit 1 MiB"));
    actor_mut(&mut group, "ghost").memory_limit_mb = 2;
    let second = super::tick_detection(&f.home, &group).expect("tick 2");
    assert!(
        second.is_empty(),
        "same reason with a new rendered limit stays silent"
    );
    assert_eq!(memory_notices(&f.home, &group.group_id).len(), 1);
}

#[test]
fn changed_outcome_same_actor_re_notifies_red_on_4881() {
    // Same actor: unavailable then over-held. On 4881 the single per-actor
    // mark set by the unavailable tick swallows the over tick (the MUST2
    // window-consumption bug); fixed per-class marks let it through.
    let (f, mut group) = worker_fixture();
    actor_mut(&mut group, "worker").profile_id = MISSING_PROFILE.into();
    assert_eq!(
        super::tick_detection(&f.home, &group)
            .expect("tick 1")
            .len(),
        1
    );
    actor_mut(&mut group, "worker").profile_id = String::new();
    let second = super::tick_detection(&f.home, &group).expect("tick 2");
    assert_eq!(
        second.len(),
        1,
        "unavailable -> over-held is a state change"
    );
    let notices = memory_notices(&f.home, &group.group_id);
    assert_eq!(notices.len(), 2);
    assert_eq!(event_actor_of(&notices[1]), "worker");
    assert_eq!(outcome_of(&notices[1]), "over_limit");
}

#[test]
fn changed_outcome_across_actors_notifies_preserved() {
    // Cross-actor control: ghost's mark never covered worker even on 4881
    // (per-actor keys), so this notifies on BOTH bases. Pins that independence.
    let (f, mut group) = ghost_fixture();
    let first = super::tick_detection(&f.home, &group).expect("tick 1");
    assert_eq!(first.len(), 1);
    actor_mut(&mut group, "ghost").enabled = false;
    actor_mut(&mut group, "worker").enabled = true;
    let second = super::tick_detection(&f.home, &group).expect("tick 2");
    assert_eq!(second.len(), 1, "marks never leak across actor IDs");
    let notices = memory_notices(&f.home, &group.group_id);
    assert_eq!(notices.len(), 2);
    assert_eq!(event_actor_of(&notices[1]), "worker");
    assert_eq!(outcome_of(&notices[1]), "over_limit");
}

#[test]
fn recovery_clears_unavailable_mark_only_red_on_4881() {
    let (f, mut group) = worker_fixture();
    actor_mut(&mut group, "worker").profile_id = MISSING_PROFILE.into();
    assert_eq!(
        super::tick_detection(&f.home, &group)
            .expect("tick 1")
            .len(),
        1
    );
    actor_mut(&mut group, "worker").profile_id = String::new();
    assert_eq!(
        super::tick_detection(&f.home, &group)
            .expect("tick 2")
            .len(),
        1
    );
    actor_mut(&mut group, "worker").memory_limit_mb = 4096;
    let healthy = super::tick_detection(&f.home, &group).expect("recovery tick");
    assert!(
        healthy.is_empty(),
        "recovery itself is silent (no-noise rule)"
    );
    actor_mut(&mut group, "worker").profile_id = MISSING_PROFILE.into();
    actor_mut(&mut group, "worker").memory_limit_mb = 1;
    let renotify = super::tick_detection(&f.home, &group).expect("tick 3");
    assert_eq!(renotify.len(), 1, "post-recovery failure is a new state");
    actor_mut(&mut group, "worker").profile_id = String::new();
    let over = super::tick_detection(&f.home, &group).expect("tick 4");
    assert!(
        over.is_empty(),
        "over window must survive an intervening recovery"
    );
}

#[test]
fn spent_over_window_survives_unavailable_interleaving_red_on_4881() {
    let (f, mut group) = worker_fixture();
    assert_eq!(
        super::tick_detection(&f.home, &group)
            .expect("over tick")
            .len(),
        1
    );
    actor_mut(&mut group, "worker").profile_id = MISSING_PROFILE.into();
    let unavailable = super::tick_detection(&f.home, &group).expect("unavailable tick");
    assert_eq!(
        unavailable.len(),
        1,
        "unavailable during a spent over window is its own change"
    );
    actor_mut(&mut group, "worker").profile_id = String::new();
    let over = super::tick_detection(&f.home, &group).expect("over tick 2");
    assert!(
        over.is_empty(),
        "over inside its already-spent window stays suppressed"
    );
}

#[test]
fn over_after_window_expiry_emits_preserved() {
    let (f, group) = worker_fixture();
    assert_eq!(
        super::tick_detection(&f.home, &group)
            .expect("tick 1")
            .len(),
        1
    );
    age_mark_class(&f.home, &group.group_id, "worker", "over", WINDOW_AGO_SECS);
    let second = super::tick_detection(&f.home, &group).expect("tick 2");
    assert_eq!(
        second.len(),
        1,
        "over re-notifies after its own window expires"
    );
    assert_eq!(memory_notices(&f.home, &group.group_id).len(), 2);
}

#[test]
fn valid_over_held_control_emits_preserved() {
    let (f, group) = worker_fixture();
    let events = super::tick_detection(&f.home, &group).expect("tick");
    assert_eq!(events.len(), 1);
    let notices = memory_notices(&f.home, &group.group_id);
    assert_eq!(outcome_of(&notices[0]), "over_limit");
    assert_eq!(notices[0]["data"]["context"]["action"], "detection_only");
}
