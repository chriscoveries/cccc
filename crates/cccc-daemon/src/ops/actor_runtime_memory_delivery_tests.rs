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
