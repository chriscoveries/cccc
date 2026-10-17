//! Real admission gates and an isolated ACP subprocess; no provider credentials.
use super::*;
use crate::ops::actor_delivery::DeliveryJob;
use crate::ops::actor_delivery_worker::{BatchOutcome, process_batch};
use crate::ops::local_headless;
use cccc_contracts::{Event, GroupState, RuntimeMode};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

type Key = (PathBuf, String, String);

fn checkpoints() -> &'static Mutex<HashMap<Key, mpsc::Sender<()>>> {
    static CHECKPOINTS: OnceLock<Mutex<HashMap<Key, mpsc::Sender<()>>>> = OnceLock::new();
    CHECKPOINTS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn before_start_permit(home: &HomeLayout, group: &str, actor: &str) {
    let key = (
        home.root().to_path_buf(),
        group.to_owned(),
        actor.to_owned(),
    );
    let sender = checkpoints().lock().expect("checkpoints").remove(&key);
    if let Some(sender) = sender {
        let _ = sender.send(());
    }
}

struct Checkpoint {
    key: Key,
    receiver: mpsc::Receiver<()>,
}

impl Checkpoint {
    fn new(f: &Fixture) -> Self {
        let key = (
            f.home.root().to_path_buf(),
            f.group.group_id.clone(),
            "worker".into(),
        );
        let (sender, receiver) = mpsc::channel();
        assert!(
            checkpoints()
                .lock()
                .expect("checkpoints")
                .insert(key.clone(), sender)
                .is_none()
        );
        Self { key, receiver }
    }
}

impl Drop for Checkpoint {
    fn drop(&mut self) {
        checkpoints().lock().expect("checkpoints").remove(&self.key);
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    home: HomeLayout,
    group: GroupDoc,
    workspace: PathBuf,
}

impl Fixture {
    fn new(linked: bool) -> Self {
        let temp = tempfile::tempdir().expect("fixture");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("initialize home");
        let store = GroupStore::new(home.clone()).expect("store");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let executable = temp.path().join("kimi");
        std::fs::write(
            &executable,
            include_str!("codex_voice_analyst/native_acp/candidate_fixture.py"),
        )
        .expect("isolated ACP fixture");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
            .expect("fixture executable");
        let group = store.create("delivery admission", "").expect("group");
        let mut group = cccc_core::group_scope::attach(
            &store,
            &group.group_id,
            cccc_core::scope::detect(&workspace).expect("scope"),
        )
        .expect("attach scope");
        let mut actor = Actor::new("worker");
        actor.runtime = ActorRuntime::Kimi;
        actor.runtime_mode = RuntimeMode::Acp;
        actor.command = vec![executable.to_string_lossy().into_owned()];
        actor.env = BTreeMap::from([
            (
                "HOME".into(),
                temp.path().join("provider").to_string_lossy().into_owned(),
            ),
            (
                "KIMI_CODE_HOME".into(),
                temp.path()
                    .join("provider/kimi")
                    .to_string_lossy()
                    .into_owned(),
            ),
            ("ACP_FIXTURE_CLI".into(), "/bin/true".into()),
        ]);
        actor.enabled = true;
        actor.normalize_runtime_constraints();
        if linked {
            let profile = json!({"name":"delivery profile","runtime":"kimi","runtime_mode":"acp",
                "command":actor.command,"env":actor.env});
            let profile = cccc_core::profiles::ProfileStore::new(home.clone())
                .expect("profiles")
                .upsert(profile.as_object().expect("profile").clone(), None)
                .expect("save profile");
            actor = actor_profile_runtime::link(&home, &actor, profile["id"].as_str().expect("id"))
                .expect("link profile");
        }
        group.actors.push(actor);
        group.running = true;
        group.state = GroupState::Active;
        store.save(&group).expect("save group");
        apply(&home, &group, "worker", "actor.start").expect("start fixture");
        Self {
            _temp: temp,
            home,
            group,
            workspace,
        }
    }

    fn job(&self) -> DeliveryJob {
        let mut event = Event::new("chat.message", &self.group.group_id);
        event.by = "user".into();
        event.data = json!({"to":["worker"],"text":"fixture task","message_mode":"send"})
            .as_object()
            .expect("event data")
            .clone();
        DeliveryJob {
            home: self.home.clone(),
            group: self.group.clone(),
            actor: self.group.actors[0].clone(),
            event,
        }
    }

    fn materialize(&self) {
        assert_eq!(
            local_headless::submit_batch(
                &self.home,
                &self.group,
                &self.group.actors[0],
                &[self.job().event],
                &AtomicBool::new(false),
            ),
            local_headless::BatchSubmission::Accepted,
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !local_headless::ready_for_mail_notice(&self.group.group_id, "worker") {
            assert!(std::time::Instant::now() < deadline, "idle fixture");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn receipt(&self) -> PathBuf {
        crate::ops::runtime_session::native_acp::receipt_path(
            &self.home,
            &self.group.group_id,
            "worker",
            ActorRuntime::Kimi,
        )
        .expect("receipt path")
    }

    fn session_id(&self) -> String {
        let receipt: Value = cccc_core::fs::read_json(&self.receipt()).expect("native receipt");
        receipt["provider_session_id"]
            .as_str()
            .expect("native session id")
            .to_owned()
    }

    fn candidate(&self) -> local_headless::memory::MemoryCandidate {
        local_headless::memory::memory_candidate(&self.group.group_id, "worker")
            .expect("owned provider")
    }

    fn requests(&self, method: &str) -> usize {
        std::fs::read_to_string(self.workspace.join("candidate_requests.jsonl"))
            .expect("requests")
            .lines()
            .filter(|line| {
                serde_json::from_str::<Value>(line).expect("RPC JSON")["method"] == method
            })
            .count()
    }

    fn replace_while_admitted(&self) -> std::io::Result<()> {
        let mut actor =
            environment::resolve_launch_actor(&self.home, &self.group, &self.group.actors[0])
                .map_err(|error| std::io::Error::other(error.message))?;
        actor.env = environment::launch_env(&self.home, &self.group, &actor);
        local_headless::start(&self.home, &self.group, &actor)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = local_headless::stop(&self.group.group_id, "worker");
        let _ = cccc_runtime::stop(&self.group.group_id, "worker");
    }
}

enum WorkerResult {
    Start(Result<bool, OpError>),
    Delivery(BatchOutcome, usize),
}

fn exercise_cancelled_waiter(delivery_worker: bool) {
    let f = Fixture::new(false);
    f.materialize();
    let old = f.candidate();
    let session_id = f.session_id();
    let permit = crate::runtime_start_gate::permit(&f.home).expect("maintenance permit");
    local_headless::stop(&f.group.group_id, "worker").expect("retire old provider");
    let guard = local_headless::hold_start_guard_for_test(&f.group.group_id, "worker")
        .expect("maintenance start guard");
    let checkpoint = Checkpoint::new(&f);
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = Arc::clone(&cancelled);
    let job = f.job();
    let worker = std::thread::spawn(move || {
        if delivery_worker {
            let mut jobs = vec![job];
            let outcome = process_batch(&mut jobs, &mut String::new(), &worker_cancelled);
            WorkerResult::Delivery(outcome, jobs.len())
        } else {
            WorkerResult::Start(start_for_delivery(
                &job.home,
                &job.group,
                &job.actor.id,
                &worker_cancelled,
            ))
        }
    });
    let reached_admission = checkpoint.receiver.recv_timeout(Duration::from_secs(2));
    // Release both gates even if the old worker regresses to stopping while it
    // waits. An assertion before releasing them would turn a failure into a hang.
    cancelled.store(true, Ordering::Release);
    drop(guard);
    let replacement = f.replace_while_admitted();
    let replaced = local_headless::memory::memory_candidate(&f.group.group_id, "worker");
    drop(permit);
    let result = worker.join().expect("old delivery worker");
    reached_admission
        .expect("old worker reached the real runtime permit while StartGuard was held");
    replacement.expect("replacement started under maintenance admission");
    let replaced = replaced.expect("replacement owner");
    assert_ne!(replaced.generation, old.generation);
    assert_eq!(
        f.candidate(),
        replaced,
        "old worker cannot retire the replacement"
    );
    assert_eq!(
        f.session_id(),
        session_id,
        "the exact saved conversation remains selected"
    );
    match result {
        WorkerResult::Start(result) => {
            assert!(!result.expect("cancellation is not a launch error"))
        }
        WorkerResult::Delivery(outcome, pending) => {
            assert_eq!(outcome, BatchOutcome::Retry);
            assert_eq!(pending, 1, "cancellation leaves delivery pending");
        }
    }
    assert!(
        start_for_delivery(&f.home, &f.group, "worker", &AtomicBool::new(false))
            .expect("new worker idempotent Start")
    );
    assert_eq!(f.candidate(), replaced);
    assert_eq!(
        f.requests("session/new"),
        1,
        "neither waiter creates a fresh session"
    );
    assert_eq!(
        f.requests("session/load"),
        1,
        "only maintenance resumes the provider"
    );
    assert_eq!(
        f.requests("session/prompt"),
        1,
        "cancelled delivery never reaches the provider"
    );
}

#[test]
fn cancelled_auto_start_waits_for_maintenance_and_preserves_replacement() {
    exercise_cancelled_waiter(false);
}

#[test]
fn cancelled_delivery_worker_retries_without_settling_jobs_or_stopping_replacement() {
    exercise_cancelled_waiter(true);
}

#[test]
fn closed_runtime_gate_preserves_the_managed_owner_and_cancellation_is_not_an_error() {
    let f = Fixture::new(false);
    let before = f.candidate();
    crate::runtime_start_gate::prevent(&f.home).expect("close runtime gate");
    let denied = start_for_delivery(&f.home, &f.group, "worker", &AtomicBool::new(false));
    let cancelled = start_for_delivery(&f.home, &f.group, "worker", &AtomicBool::new(true));
    crate::runtime_start_gate::allow(&f.home).expect("restore runtime gate");
    assert_eq!(
        denied.expect_err("shutdown blocks delivery start").code,
        "runtime_shutting_down"
    );
    assert!(!cancelled.expect("cancellation is a normal retry"));
    assert_eq!(f.candidate(), before);
    assert_eq!(f.requests("session/new"), 1);
}

#[test]
fn delivery_start_honors_required_resume_without_falling_back_to_a_fresh_session() {
    let f = Fixture::new(false);
    f.materialize();
    let required = crate::ops::actor_memory::RequiredResume {
        generation: f.candidate().generation,
        runtime: ActorRuntime::Kimi,
        session_id: f.session_id(),
    };
    local_headless::stop(&f.group.group_id, "worker").expect("stop fixture");
    crate::ops::actor_memory::require_resume(&f.home, &f.group.group_id, "worker", &required)
        .expect("persist resume requirement");
    std::fs::remove_file(f.receipt()).expect("receipt disappeared during maintenance");
    let before = std::fs::read(f.workspace.join("candidate_requests.jsonl")).expect("requests");
    let error = start_for_delivery(&f.home, &f.group, "worker", &AtomicBool::new(false))
        .expect_err("missing required receipt must block auto-wake");
    assert_eq!(error.code, RUNTIME_RESUME_FAILED);
    assert!(!local_headless::running(&f.group.group_id, "worker"));
    assert_eq!(
        std::fs::read(f.workspace.join("candidate_requests.jsonl")).expect("same requests"),
        before
    );
    assert_eq!(
        crate::ops::actor_memory::required_resume(&f.home, &f.group.group_id, "worker")
            .expect("retained requirement"),
        Some(required),
    );
}

#[test]
fn delivery_start_uses_current_profile_and_preserves_native_to_pty_transition() {
    let f = Fixture::new(true);
    let before = f.candidate();
    let actor = &f.group.actors[0];
    let profile = json!({"id":actor.profile_id,"name":"changed delivery profile","runtime":"custom","runtime_mode":"default",
        "runner":"pty","command":["/bin/sh","-c","exec sleep 60"]});
    cccc_core::profiles::ProfileStore::new(f.home.clone())
        .expect("profiles")
        .upsert(profile.as_object().expect("profile").clone(), None)
        .expect("change effective backend");
    assert!(
        start_for_delivery(&f.home, &f.group, "worker", &AtomicBool::new(false))
            .expect("healthy owner remains idempotent across backend changes")
    );
    assert_eq!(f.candidate(), before);
    local_headless::stop(&f.group.group_id, "worker").expect("retire previous owner");
    assert!(
        start_for_delivery(&f.home, &f.group, "worker", &AtomicBool::new(false))
            .expect("auto-start current PTY backend without reacquiring the permit")
    );
    assert!(status(&f.group.group_id, "worker").is_some_and(|status| status.running));
    assert!(local_headless::registered_running(&f.group.group_id, "worker").is_none());
    assert_eq!(
        f.requests("session/new"),
        1,
        "current Profile does not restart stale ACP settings"
    );
}

// Append to actor_runtime_memory_delivery_tests.rs at 9210; reuse real Fixture.

fn required_resume_for_fixture(f: &Fixture) -> crate::ops::actor_memory::RequiredResume {
    f.materialize();
    let candidate = f.candidate();
    let required = crate::ops::actor_memory::RequiredResume {
        generation: candidate.generation,
        runtime: ActorRuntime::Kimi,
        session_id: f.session_id(),
    };
    crate::ops::actor_memory::require_resume(&f.home, &f.group.group_id, "worker", &required)
        .expect("durable exact-resume obligation");
    required
}

fn fixture_changed_to_custom(f: &Fixture) -> GroupDoc {
    let store = GroupStore::new(f.home.clone()).expect("store");
    store.mutate(&f.group.group_id, |group| {
        let actor = &mut group.actors[0];
        actor.runtime = ActorRuntime::Custom;
        actor.runtime_mode = RuntimeMode::Default;
        actor.command = vec!["python3".into(), "-u".into(), "-c".into(),
            "import sys; from pathlib import Path; Path('custom-launched').write_text('started'); print('offline custom ready', flush=True); [None for line in sys.stdin]".into()];
        actor.normalize_runtime_constraints();
        Ok(())
    }).expect("changed next-launch settings");
    store
        .load(&f.group.group_id)
        .expect("changed actor snapshot")
}

#[test]
fn required_resume_blocks_raw_custom_manual_and_delivery_start_until_explicit_new_session() {
    let f = Fixture::new(false);
    let required = required_resume_for_fixture(&f);
    let before = f.candidate();
    let changed = fixture_changed_to_custom(&f);
    let receipt = std::fs::read(f.receipt()).expect("original native receipt");
    // A healthy current owner remains idempotent despite next-launch drift.
    apply(&f.home, &changed, "worker", "actor.start").expect("idempotent current owner");
    assert!(
        start_for_delivery(&f.home, &changed, "worker", &AtomicBool::new(false))
            .expect("idempotent delivery owner")
    );
    assert_eq!(f.candidate(), before);
    assert_eq!(
        std::fs::read(f.receipt()).expect("preserved receipt"),
        receipt
    );
    let restart = apply(&f.home, &changed, "worker", "actor.restart")
        .expect_err("runtime drift must fail before retiring the healthy provider");
    assert_eq!(restart.code, RUNTIME_RESUME_FAILED);
    assert_eq!(
        f.candidate(),
        before,
        "rejected restart must not stop the old owner"
    );
    local_headless::stop(&changed.group_id, "worker").expect("stop current owner");
    let start = apply(&f.home, &changed, "worker", "actor.start")
        .expect_err("raw start must not bypass the saved native session");
    assert_eq!(start.code, RUNTIME_RESUME_FAILED);
    let delivery = start_for_delivery(&f.home, &changed, "worker", &AtomicBool::new(false))
        .expect_err("automatic raw start must honor the same obligation");
    assert_eq!(delivery.code, RUNTIME_RESUME_FAILED);
    assert!(
        !f.workspace.join("custom-launched").exists(),
        "neither path may execute the Custom program"
    );
    assert!(cccc_runtime::status(&changed.group_id, "worker").is_err());
    assert_eq!(
        crate::ops::actor_memory::required_resume(&f.home, &changed.group_id, "worker")
            .expect("retained obligation"),
        Some(required)
    );
    assert_eq!(
        std::fs::read(f.receipt()).expect("retained native receipt"),
        receipt
    );
    apply(&f.home, &changed, "worker", "actor.new_session")
        .expect("explicit new session authorizes a fresh Custom launch");
    let until = std::time::Instant::now() + Duration::from_secs(5);
    while !f.workspace.join("custom-launched").exists() {
        assert!(
            std::time::Instant::now() < until,
            "explicit fresh Custom process must run"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        crate::ops::actor_memory::required_resume(&f.home, &changed.group_id, "worker")
            .expect("explicitly retired obligation")
            .is_none()
    );
    assert!(
        cccc_runtime::status(&changed.group_id, "worker")
            .expect("new terminal")
            .running
    );
    assert_eq!(
        f.requests("session/new"),
        1,
        "Custom reset must not create another native conversation"
    );
}

#[test]
fn corrupt_memory_requirement_blocks_every_new_raw_start_without_becoming_a_fresh_fallback() {
    let f = Fixture::new(false);
    let _required = required_resume_for_fixture(&f);
    let changed = fixture_changed_to_custom(&f);
    local_headless::stop(&changed.group_id, "worker").expect("stop owner");
    let path = GroupStore::new(f.home.clone())
        .expect("store")
        .state_dir(&changed.group_id)
        .expect("state")
        .join("actor_memory_required/worker.json");
    std::fs::write(&path, b"{ invalid").expect("corrupt obligation");
    for delivery in [false, true] {
        let error = if delivery {
            start_for_delivery(&f.home, &changed, "worker", &AtomicBool::new(false))
                .map(|_| ())
                .expect_err("corrupt automatic start")
        } else {
            apply(&f.home, &changed, "worker", "actor.start")
                .map(|_| ())
                .expect_err("corrupt manual start")
        };
        assert_eq!(error.code, RUNTIME_RESUME_FAILED);
        assert!(!f.workspace.join("custom-launched").exists());
        assert_eq!(
            std::fs::read(&path).expect("unchanged corrupt obligation"),
            b"{ invalid".to_vec()
        );
    }
}

#[path = "actor_runtime_manual_admission_tests.rs"]
pub(super) mod manual_admission;

// ---- MUST3 (tests-only): managed accepted/rejected submit contracts ----
// Real admission through the isolated ACP fixture; no synthetic activity.

/// The fixture logs every provider RPC; only `session/prompt` is a submit —
/// initialization traffic (`initialize`, `session/new`, ...) must not count
/// as a submit record (MUST3 review: a raw line count proves nothing).
fn count_prompts(f: &Fixture) -> usize {
    std::fs::read_to_string(f.workspace.join("candidate_requests.jsonl"))
        .map(|body| {
            body.lines()
                .filter(|line| {
                    serde_json::from_str::<Value>(line).expect("RPC JSON")["method"]
                        == "session/prompt"
                })
                .count()
        })
        .unwrap_or(0)
}

/// Pin: a real ACCEPTED managed submit reaches the provider as a prompt —
/// baseline counted AFTER fixture initialization, so the delta is the submit
/// record itself. GREEN today; the managed half of the accepted-submit
/// contract, independent of the (unimplemented) input seq.
#[test]
fn accepted_managed_submit_reaches_the_provider() {
    let f = Fixture::new(false);
    let baseline = count_prompts(&f);
    f.materialize(); // asserts BatchSubmission::Accepted, then waits idle
    assert_eq!(
        count_prompts(&f),
        baseline + 1,
        "exactly one session/prompt must arrive for the accepted managed submit"
    );
}

/// Control: a submit cancelled before admission is Deferred and generates NO
/// provider traffic — rejected/cancel-before-accept MUST NOT count as input
/// and must not veto a later legitimate stop.
#[test]
fn cancelled_before_managed_admission_defers_without_provider_traffic() {
    let f = Fixture::new(false);
    let cancelled = AtomicBool::new(true);
    assert_eq!(
        local_headless::submit_batch(
            &f.home,
            &f.group,
            &f.group.actors[0],
            &[f.job().event],
            &cancelled,
        ),
        local_headless::BatchSubmission::Deferred,
        "cancelled-before-admission must defer, not deliver"
    );
    assert_eq!(
        count_prompts(&f),
        0,
        "no session/prompt may appear for a cancelled managed submit"
    );
}

/// Owner MUST3 managed-accept-input behavior, with BOTH vacuity guards the
/// review required. Structure: an UNARMED same-flow control first (must
/// Restart with commit reached — proving the fixture reaches close at all),
/// then re-materialize + re-observe, then the ARMED run asserting the veto.
/// On current source the armed half is RED: nothing records the accepted
/// input, so maintenance closes and replaces the provider session. The
/// control narrows false-pass explanations — every gate between the hook and
/// close is identical for both halves, so a gate-caused Skipped would have
/// stopped the control too — but it does NOT prove every armed gate
/// unchanged (a drift introduced by the completed turn itself would evade
/// the control), so the baseline run remains the authority: it must show the
/// armed half failing on the intended outcome (Restarted where Skipped is
/// asserted) with the control green first; any other shape is a fixture
/// defect, not MUST3 evidence.
#[test]
fn real_accepted_managed_submit_before_native_maintenance_vetoes_the_restart() {
    let f = Fixture::new(false);
    f.materialize(); // accepted + completed (idle) — provider session established

    // CONTROL (unarmed): the identical call must reach close and restart.
    let control_expected = f.candidate();
    let control_commits = std::sync::atomic::AtomicUsize::new(0);
    let control_outcome = crate::ops::local_headless::memory::restart_for_memory_with_commit(
        &f.home,
        &f.group.group_id,
        "worker",
        &control_expected,
        |_, _, _| Ok(true),
        || {
            control_commits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        },
    )
    .expect("unarmed control maintenance returns");
    assert_eq!(
        control_outcome,
        crate::ops::local_headless::memory::MemoryRestartOutcome::Restarted,
        "unarmed control proves the fixture reaches close — a gate-caused \
         Skipped here means the armed half below would be vacuous"
    );
    assert_eq!(
        control_commits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the control's commit must be reached"
    );

    // Re-materialize the replacement session and re-observe the candidate so
    // the armed half differs from the control ONLY by the interposed input.
    f.materialize();
    let expected = f.candidate();
    let session_before = f.session_id();
    // Oracle note (oc-muse secondary): the native commit has no single
    // persistence file like the PTY side's cooldown, so this half rests on
    // the commit-call counter + session-id preservation, not on a
    // would-be-real write.
    let commit_calls = std::sync::atomic::AtomicUsize::new(0);
    let _arm = crate::ops::actor_memory::must3_gap_tests::arm(
        f.home.root().to_path_buf(),
        f.group.group_id.clone(),
        "worker".into(),
    );
    let outcome = crate::ops::local_headless::memory::restart_for_memory_with_commit(
        &f.home,
        &f.group.group_id,
        "worker",
        &expected,
        |_, _, _| Ok(true),
        || {
            commit_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        },
    )
    .expect("armed maintenance returns");
    assert_eq!(
        outcome,
        crate::ops::local_headless::memory::MemoryRestartOutcome::Skipped,
        "accepted input newer than the maintenance observation must veto the restart"
    );
    assert_eq!(
        f.session_id(),
        session_before,
        "the owned provider session must be untouched"
    );
    assert_eq!(
        commit_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "commit must never be reached when the restart is vetoed"
    );
}
