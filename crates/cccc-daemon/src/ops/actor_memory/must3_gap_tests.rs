//! MUST3 cfg(test) gap interposition — tests-only slice, no production behavior.
//!
//! The submit-in-gap regression needs a REAL submit to land inside
//! `restart_for_memory_pty` between its final quiet recheck and the stop.
//! Recording synthetic activity or holding `active_turn` would not establish
//! that race (and would broaden the stop policy), so this module performs the
//! actor's own real submission path exactly once, through an armed target the
//! test registers first — the same one-shot checkpoint pattern the delivery
//! tests use.
//!
//! Managed note: `restart_for_memory_pty` skips native-ACP actors up front
//! (`is_native_acp`, memory_pty.rs:36) and a non-native managed actor has no
//! PTY activity snapshot to quiet-check, so the managed actor's maintenance
//! stop lives in the native path (`restart_for_memory_with_commit`). The
//! accepted/rejected managed-submit contracts are exercised in
//! `actor_runtime_memory_delivery_tests` instead; a managed instance of the
//! gap test needs that native path's interposition seam and is reported, not
//! faked.
use super::*;
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, OnceLock};

type Target = (std::path::PathBuf, String, String);

fn armed() -> &'static Mutex<Option<Target>> {
    static ARMED: OnceLock<Mutex<Option<Target>>> = OnceLock::new();
    ARMED.get_or_init(|| Mutex::new(None))
}

/// Arm exactly one submit-in-gap for this (home, group, actor); called by the
/// test before it enters the maintenance flow.
pub(crate) fn arm(home: std::path::PathBuf, group_id: String, actor_id: String) {
    let mut slot = armed().lock().expect("must3 gap arm");
    assert!(slot.is_none(), "one armed gap submit at a time");
    *slot = Some((home, group_id, actor_id));
}

/// Invoked from `restart_for_memory_pty` after its final quiet recheck and
/// before `commit()`: performs the armed actor's real submit once, then
/// disarms. With no armed target this is a no-op, so every other test in the
/// crate runs through the hook untouched.
pub(crate) fn interpose_submit(home: &HomeLayout, group: &GroupDoc, actor_id: &str) {
    let target = {
        let mut slot = armed().lock().expect("must3 gap arm");
        match slot.take() {
            Some(target) => target,
            None => return,
        }
    };
    let (home_root, group_id, armed_actor) = target;
    assert_eq!(home.root(), home_root, "armed for a different home");
    assert_eq!(group.group_id, group_id, "armed for a different group");
    assert_eq!(actor_id, armed_actor, "armed for a different actor");
    let Some(actor) = group.actors.iter().find(|actor| actor.id == actor_id) else {
        panic!("MUST3 gap submit armed for an actor the fresh group does not carry");
    };
    // Real accepted submission through the actor's own entrypoint. For the
    // PTY-side fixture this is the terminal submit path (input_gate guard,
    // `record_input` on completion) — the flow `restart_for_memory_pty` can
    // actually reach; see the managed note in the module docs.
    assert!(
        crate::ops::actor_delivery::submit_terminal_text(
            &group.group_id,
            actor,
            "MUST3 real submit landing in the quiet-to-stop gap",
            &AtomicBool::new(false),
        ),
        "the gap submit itself must be accepted for the regression to be real"
    );
}
