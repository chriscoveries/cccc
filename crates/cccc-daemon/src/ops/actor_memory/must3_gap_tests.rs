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

/// RAII arm: dropped (including while a test unwinds from a panic) it
/// disarms, so an early exit can never leave the one-shot slot armed for an
/// unrelated maintenance call.
pub(crate) struct ArmGuard {
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
/// disarms on drop. One armed target at a time is asserted.
pub(crate) fn arm(home: std::path::PathBuf, group_id: String, actor_id: String) -> ArmGuard {
    let mut slot = armed().lock().expect("must3 gap arm");
    assert!(slot.is_none(), "one armed gap submit at a time");
    *slot = Some((home.clone(), group_id.clone(), actor_id.clone()));
    ArmGuard {
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
            Some(target) if target.1 == group_id && target.2 == actor_id => slot.take(),
            _ => return,
        }
    };
    let Some((home_root, armed_group, armed_actor)) = armed_target else {
        return;
    };
    assert_eq!(home.root(), home_root, "armed for a different home");
    assert_eq!(group_id, armed_group);
    assert_eq!(actor_id, armed_actor);
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
            std::time::thread::sleep(std::time::Duration::from_millis(10));
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
