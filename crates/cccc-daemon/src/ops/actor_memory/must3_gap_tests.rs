//! MUST3 cfg(test) gap interposition — tests-only slice, no production behavior.
//!
//! The submit-in-gap regressions need a REAL accepted submit to land inside the
//! maintenance flows at the exact window their fix must close. Recording
//! synthetic activity or holding `active_turn` would not establish that race
//! (and would broaden the stop policy), so this module performs the actor's
//! own real submission path exactly once, through an armed target the test
//! registers first.
//!
//! **Ordering boundary (load-bearing):** both interposition call sites run
//! *before any gate the submit itself needs* — the PTY hook sits after the
//! final quiet recheck and before `commit()`/`stop_registered`, with no
//! `input_gate` held; the native hook sits after the start-gate permit and
//! *before* `StartGuard::acquire`, so a managed `submit_batch` (which
//! `try_acquire`s that same guard) can genuinely ACCEPT here rather than
//! defer. A future production recheck must take its guard only AFTER these
//! points; a seam frozen inside the very gate it needs would deadlock
//! (session→input_gate inversion, or StartGuard re-entry).
use super::*;
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, OnceLock};

type Target = (std::path::PathBuf, String, String);

fn armed() -> &'static Mutex<Option<Target>> {
    static ARMED: OnceLock<Mutex<Option<Target>>> = OnceLock::new();
    ARMED.get_or_init(|| Mutex::new(None))
}

/// Process-global serialization shared by BOTH arm users (the PTY gap test
/// and the managed native-flow test), held from `arm()` through the whole
/// maintenance call via the ArmGuard's lifetime. The two tests live in the
/// same lib test binary and run in parallel by default; without this, the
/// second `arm()` would panic on the one-slot assert and read exactly like a
/// regression. Blocking (not try-lock): an overlap waits its turn instead of
/// failing — the full suite must pass on default parallelism, not only under
/// `--test-threads=1`. Lock order: this serial gate is acquired FIRST and the
/// armed-slot lock is only ever taken while it is held; no other code path
/// takes them in the opposite order, so there is no inversion.
///
/// POISON RECOVERY (deliberate, serial gate ONLY): an intended-RED assertion
/// panics while an ArmGuard is live, so this `std::sync::Mutex` becomes
/// poisoned on every genuine failure — and a fixture that then failed the
/// NEXT arm on `expect("must3 serial gate")` would convert one real MUST3
/// finding into unrelated fixture failures. The gate protects no data, only
/// ownership, so recovery via `PoisonError::into_inner` is sound: a poisoned
/// serial gate means "an earlier test panicked", which is information the
/// fixture must survive, not a fault. The armed-slot mutex stays fail-loud
/// on purpose: its critical section is a bare `Option<Target>` touched under
/// this gate, so a poison there would mean corruption we cannot prove
/// harmless — keep it loud rather than paper over it.
fn serialization() -> &'static Mutex<()> {
    static SERIAL: OnceLock<Mutex<()>> = OnceLock::new();
    SERIAL.get_or_init(|| Mutex::new(()))
}

/// RAII arm: holds the shared serialization gate (see `serialization`) for
/// its whole lifetime — i.e. from arm through the maintenance call — and on
/// drop (including while a test unwinds from a panic) disarms only if the
/// slot is still its own, so an early exit can never leave the one-shot
/// armed for an unrelated maintenance call.
///
/// ORDER DEPENDENCE (load-bearing): the disarm runs in `Drop::drop`, which
/// always executes BEFORE the fields drop, so the slot is cleared while the
/// serial gate is still held — never after. Releasing the serial gate first
/// would open a window where the next arm acquires serial, then trips the
/// one-slot assert on the dying guard's still-armed slot. Field order here
/// is therefore not cosmetic: keep the disarm in `Drop::drop` (as written)
/// rather than relying on field-drop order.
pub(crate) struct ArmGuard {
    _serial: std::sync::MutexGuard<'static, ()>,
    home: std::path::PathBuf,
    group_id: String,
    actor_id: String,
}

impl Drop for ArmGuard {
    fn drop(&mut self) {
        let mut slot = armed().lock().expect("must3 gap arm");
        if let Some((home, group_id, actor_id)) = slot.as_ref()
            && home == &self.home
            && group_id == &self.group_id
            && actor_id == &self.actor_id
        {
            *slot = None;
        }
    }
}

/// Arm exactly one submit-in-gap for this (home, group, actor); the guard
/// serializes arm-through-maintenance across tests and disarms on drop. One
/// armed target at a time is asserted (a second arm now means a bug in the
/// serialization, not a race, because the caller would be holding the gate).
pub(crate) fn arm(home: std::path::PathBuf, group_id: String, actor_id: String) -> ArmGuard {
    // Serial gate: recover poison (see `serialization`) — an earlier intended
    // RED panic must not cascade into fixture failures here.
    let serial = serialization()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Armed slot: fail-loud on poison by design (see `serialization`).
    let mut slot = armed().lock().expect("must3 gap arm");
    assert!(slot.is_none(), "one armed gap submit at a time");
    *slot = Some((home.clone(), group_id.clone(), actor_id.clone()));
    ArmGuard {
        _serial: serial,
        home,
        group_id,
        actor_id,
    }
}

/// Invoked from both maintenance flows at their gap point. The armed target
/// is matched **while the lock is held and before any take**: an unrelated
/// parallel maintenance call for a different actor leaves the slot untouched
/// and simply proceeds, so it can neither steal the arm nor panic on it.
pub(crate) fn interpose_submit(home: &HomeLayout, group_id: &str, actor_id: &str) {
    let armed_target = {
        let mut slot = armed().lock().expect("must3 gap arm");
        match slot.as_ref() {
            Some(target)
                if target.0 == home.root() && target.1 == group_id && target.2 == actor_id =>
            {
                slot.take()
            }
            _ => return,
        }
    };
    let Some((home_root, armed_group, armed_actor)) = armed_target else {
        return;
    };
    debug_assert_eq!(home.root(), home_root);
    debug_assert_eq!(group_id, armed_group);
    debug_assert_eq!(actor_id, armed_actor);
    let store = cccc_core::GroupStore::new(home.clone()).expect("group store");
    let group = store.load(group_id).expect("fresh group");
    let Some(actor) = group
        .actors
        .iter()
        .find(|actor| actor.id == actor_id && actor.enabled)
    else {
        panic!("MUST3 gap submit armed for an actor the fresh group does not carry");
    };
    if crate::ops::local_headless::supports(actor) {
        // Managed path: a real accepted submission that is also COMPLETED
        // before maintenance resumes — accepted-then-completed is exactly the
        // behavior owner MUST3 must veto on.
        let mut event = cccc_contracts::Event::new("chat.message", group_id);
        event.by = "user".into();
        event.data = serde_json::json!({"to":[actor_id],
            "text":"MUST3 real managed submit landing before the native maintenance guard",
            "message_mode":"send"})
        .as_object()
        .expect("event data")
        .clone();
        assert!(
            matches!(
                crate::ops::local_headless::submit_batch(
                    home,
                    &group,
                    actor,
                    &[event],
                    &AtomicBool::new(false),
                ),
                crate::ops::local_headless::BatchSubmission::Accepted
            ),
            "the managed gap submit itself must be accepted for the regression to be real"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !crate::ops::local_headless::ready_for_mail_notice(group_id, actor_id) {
            assert!(
                std::time::Instant::now() < deadline,
                "the managed gap submit must complete, not merely be accepted"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    } else {
        // PTY path: the terminal submit (input_gate guard, `record_input` on
        // completion) — the flow restart_for_memory_pty can actually reach.
        assert!(
            crate::ops::actor_delivery::submit_terminal_text(
                group_id,
                actor,
                "MUST3 real submit landing in the quiet-to-stop gap",
                &AtomicBool::new(false),
            ),
            "the PTY gap submit itself must be accepted for the regression to be real"
        );
    }
}
