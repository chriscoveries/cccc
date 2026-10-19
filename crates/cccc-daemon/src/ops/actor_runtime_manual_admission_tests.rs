//! Manual lifecycle admission uses the real global gate and native coordinator.
//! The only hook is an exact-fixture, one-shot channel checkpoint.
use super::*;
use std::sync::atomic::AtomicU64;

#[derive(Clone, Copy, Debug)]
pub(in crate::ops::actor_runtime) enum ManualPhase {
    BeforeStop,
    BeforePermit,
}

struct Registration {
    token: u64,
    reached: mpsc::Sender<Reached>,
}

struct Reached {
    phase: ManualPhase,
    release: mpsc::Sender<()>,
}

fn manual_checkpoints() -> &'static Mutex<HashMap<Key, Registration>> {
    static CHECKPOINTS: OnceLock<Mutex<HashMap<Key, Registration>>> = OnceLock::new();
    CHECKPOINTS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(in crate::ops::actor_runtime) fn checkpoint(
    home: &HomeLayout,
    group: &str,
    actor: &str,
    phase: ManualPhase,
) {
    let key = (
        home.root().to_path_buf(),
        group.to_owned(),
        actor.to_owned(),
    );
    // Consume before waiting: subsequent lifecycle phases cannot pause again,
    // and no registry mutex is held while either thread owns an admission gate.
    let registration = manual_checkpoints()
        .lock()
        .expect("manual checkpoints")
        .remove(&key);
    if let Some(registration) = registration {
        let (release, released) = mpsc::channel();
        if registration
            .reached
            .send(Reached { phase, release })
            .is_ok()
        {
            let _ = released.recv_timeout(Duration::from_secs(30));
        }
    }
}

struct ManualCheckpoint {
    key: Key,
    token: u64,
    receiver: mpsc::Receiver<Reached>,
}

impl ManualCheckpoint {
    fn new(f: &Fixture) -> Self {
        static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);
        let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
        let key = (
            f.home.root().to_path_buf(),
            f.group.group_id.clone(),
            "worker".into(),
        );
        let (reached, receiver) = mpsc::channel();
        let previous = manual_checkpoints()
            .lock()
            .expect("manual checkpoints")
            .insert(key.clone(), Registration { token, reached });
        assert!(previous.is_none(), "one checkpoint per isolated fixture");
        Self {
            key,
            token,
            receiver,
        }
    }
}

impl Drop for ManualCheckpoint {
    fn drop(&mut self) {
        let mut checkpoints = manual_checkpoints().lock().expect("manual checkpoints");
        // A stale scope must never remove a successor's checkpoint.
        if checkpoints
            .get(&self.key)
            .is_some_and(|registration| registration.token == self.token)
        {
            checkpoints.remove(&self.key);
        }
    }
}

#[test]
fn manual_restart_waiting_on_native_maintenance_cannot_bypass_new_resume_requirement() {
    let f = Fixture::new(false);
    f.materialize();
    let candidate = f.candidate();
    let required = crate::ops::actor_memory::RequiredResume {
        generation: candidate.generation.clone(),
        runtime: ActorRuntime::Kimi,
        session_id: f.session_id(),
    };
    let checkpoint = ManualCheckpoint::new(&f);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (finish_tx, finish_rx) = mpsc::channel();
    let memory_home = f.home.clone();
    let memory_group = f.group.group_id.clone();
    let receipt = f.receipt();
    let maintenance = std::thread::spawn(move || {
        local_headless::memory::restart_for_memory(
            &memory_home,
            &memory_group,
            "worker",
            &candidate,
            |_, _, _| {
                entered_tx.send(()).map_err(std::io::Error::other)?;
                finish_rx
                    .recv_timeout(Duration::from_secs(30))
                    .map_err(std::io::Error::other)?;
                // Real preflight already accepted this exact native receipt.
                // The coordinator must persist its obligation and then fail its
                // required-only launch; it may never create a fresh session.
                std::fs::remove_file(&receipt)?;
                Ok(true)
            },
        )
    });
    let entered = entered_rx.recv_timeout(Duration::from_secs(10));
    if entered.is_err() {
        drop(finish_tx);
        let completed = maintenance.join();
        panic!("real native coordinator did not reach admission: {entered:?}; {completed:?}");
    }
    // The coordinator has captured the original effective Kimi configuration
    // while owning both real gates. Only the next manual launch changes to PTY.
    let changed = fixture_changed_to_custom(&f);
    let manual_home = f.home.clone();
    let manual = std::thread::spawn(move || {
        apply_explicit(&manual_home, &changed, "worker", "actor.restart")
    });
    let reached = checkpoint.receiver.recv_timeout(Duration::from_secs(10));
    let _ = finish_tx.send(());
    let memory_result = maintenance.join();
    // On the baseline this is BeforeStop (the stale None was already read).
    // With the correction it is BeforePermit, before reading any obligation.
    // In both cases release every barrier before assertions or joining manual.
    let phase = reached.as_ref().ok().map(|reached| reached.phase);
    if let Ok(reached) = &reached {
        let _ = reached.release.send(());
    }
    let manual_result = manual.join();
    reached.expect("manual lifecycle reached an instrumented real admission phase");
    let memory_error = memory_result
        .expect("native maintenance thread")
        .expect_err("deleted exact receipt must fail the actual native restart");
    assert!(
        crate::ops::actor_memory::is_resume_blocked(&memory_error),
        "required-only failure must remain typed: {memory_error}"
    );
    let error = manual_result
        .expect("manual lifecycle thread")
        .expect_err("manual restart must recheck the obligation after maintenance admission");
    assert_eq!(error.code, RUNTIME_RESUME_FAILED, "first phase: {phase:?}");
    assert_eq!(
        crate::ops::actor_memory::required_resume(&f.home, &f.group.group_id, "worker")
            .expect("durable obligation"),
        Some(required),
        "failed exact resume retains the original native identity"
    );
    assert!(
        !f.receipt().exists(),
        "failure injection removed the receipt"
    );
    assert!(!f.workspace.join("custom-launched").exists());
    assert!(cccc_runtime::status(&f.group.group_id, "worker").is_err());
    assert!(local_headless::memory::memory_candidate(&f.group.group_id, "worker").is_none());
    assert_eq!(f.requests("session/new"), 1);
    assert_eq!(f.requests("session/load"), 0);
    assert_eq!(f.requests("session/prompt"), 1);
}

#[test]
fn closed_start_gate_manual_restart_preserves_the_native_owner() {
    let f = Fixture::new(false);
    let before = f.candidate();
    crate::runtime_start_gate::prevent(&f.home).expect("close start gate");
    let result = apply_explicit(&f.home, &f.group, "worker", "actor.restart");
    let after = local_headless::memory::memory_candidate(&f.group.group_id, "worker");
    crate::runtime_start_gate::allow(&f.home).expect("restore start gate before assertions");
    assert_eq!(
        result.expect_err("restart refused").code,
        "runtime_shutting_down"
    );
    assert_eq!(
        after,
        Some(before),
        "refusal cannot retire the healthy owner"
    );
    assert_eq!(f.requests("session/new"), 1);
}

#[test]
fn closed_start_gate_refuses_healthy_idempotent_manual_start_without_retiring_owner() {
    let f = Fixture::new(false);
    let before = f.candidate();
    crate::runtime_start_gate::prevent(&f.home).expect("close start gate");
    let result = apply(&f.home, &f.group, "worker", "actor.start");
    let after = local_headless::memory::memory_candidate(&f.group.group_id, "worker");
    crate::runtime_start_gate::allow(&f.home).expect("restore start gate before assertions");
    assert_eq!(
        result
            .expect_err("closed gate also refuses idempotent Start")
            .code,
        "runtime_shutting_down"
    );
    assert_eq!(after, Some(before));
    assert_eq!(f.requests("session/new"), 1);
}

#[test]
fn explicit_stop_can_retire_native_owner_while_starts_remain_closed() {
    let f = Fixture::new(false);
    crate::runtime_start_gate::prevent(&f.home).expect("close start gate");
    let result = apply_explicit(&f.home, &f.group, "worker", "actor.stop");
    let still_closed = crate::runtime_start_gate::permit(&f.home).is_err();
    let owner = local_headless::memory::memory_candidate(&f.group.group_id, "worker");
    crate::runtime_start_gate::allow(&f.home).expect("restore start gate before assertions");
    result.expect("explicit stop remains available during shutdown");
    assert!(still_closed, "stop must not reopen launch admission");
    assert!(owner.is_none());
    assert_eq!(f.requests("session/new"), 1);
}

#[test]
fn group_stop_can_retire_native_owner_while_starts_remain_closed() {
    let f = Fixture::new(false);
    crate::runtime_start_gate::prevent(&f.home).expect("close start gate");
    let result = stop_group(&f.home, &f.group);
    let still_closed = crate::runtime_start_gate::permit(&f.home).is_err();
    let owner = local_headless::memory::memory_candidate(&f.group.group_id, "worker");
    crate::runtime_start_gate::allow(&f.home).expect("restore start gate before assertions");
    result.expect("group stop remains available during shutdown");
    assert!(still_closed, "group stop must not reopen launch admission");
    assert!(owner.is_none());
    assert_eq!(f.requests("session/new"), 1);
}
