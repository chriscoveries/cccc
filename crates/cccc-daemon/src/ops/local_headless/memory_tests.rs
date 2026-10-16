use super::*;
use crate::ops::{actor_memory, actor_runtime};
use cccc_contracts::{ActorRuntime, Event, RuntimeMode};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

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
        let store = GroupStore::new(home.clone()).expect("store");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let program = temp.path().join("kimi");
        std::fs::write(
            &program,
            include_str!("../codex_voice_analyst/native_acp/candidate_fixture.py"),
        )
        .expect("offline ACP fixture");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))
            .expect("executable");
        let mut group = store.create("memory maintenance", "").expect("group");
        group = cccc_core::group_scope::attach(
            &store,
            &group.group_id,
            cccc_core::scope::detect(&workspace).expect("scope"),
        )
        .expect("attach scope");
        let mut actor = Actor::new("worker");
        actor.runtime = ActorRuntime::Kimi;
        actor.runtime_mode = RuntimeMode::Acp;
        actor.command = vec![program.to_string_lossy().into_owned()];
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
            let profile = json!({"name":"memory profile","runtime":"kimi","runtime_mode":"acp",
                "command":actor.command,"env":actor.env});
            let profile = cccc_core::profiles::ProfileStore::new(home.clone())
                .expect("profiles")
                .upsert(profile.as_object().expect("profile object").clone(), None)
                .expect("save profile");
            actor = crate::ops::actor_profile_runtime::link(
                &home,
                &actor,
                profile["id"].as_str().expect("profile id"),
            )
            .expect("link profile");
        }
        group.actors.push(actor);
        group.running = true;
        group.state = GroupState::Active;
        store.save(&group).expect("save group");
        actor_runtime::apply(&home, &group, "worker", "actor.start").expect("start fixture");
        Self {
            _temp: temp,
            home,
            group,
            workspace,
        }
    }

    fn item(&self) -> std::sync::Arc<Session> {
        supervisor::lookup(&(self.group.group_id.clone(), "worker".into())).expect("live session")
    }

    fn materialize(&self) {
        let item = self.item();
        super::super::block_on_managed(async {
            let mut events = item.managed.subscribe();
            item.managed
                .start_turn(item.managed.generation(), "materialize", "fixture")
                .await
                .expect("fixture prompt");
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let event = events.recv().await.expect("event");
                    if event.message["method"] == "turn/completed" {
                        break;
                    }
                }
            })
            .await
            .expect("completed fixture prompt");
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !supervisor::ready_for_mail_notice(&self.group.group_id, "worker") {
            assert!(std::time::Instant::now() < deadline, "idle projection");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn candidate(&self) -> MemoryCandidate {
        memory_candidate(&self.group.group_id, "worker").expect("owned native provider")
    }

    fn restart(
        &self,
        candidate: &MemoryCandidate,
        admit: impl FnOnce(&GroupDoc, &Actor, &MemoryCandidate) -> io::Result<bool>,
    ) -> io::Result<MemoryRestartOutcome> {
        restart_for_memory(&self.home, &self.group.group_id, "worker", candidate, admit)
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

    fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(self.workspace.join("candidate_requests.jsonl"))
            .expect("requests")
            .lines()
            .map(|line| serde_json::from_str(line).expect("request JSON"))
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = supervisor::stop(&self.group.group_id, "worker");
    }
}

#[test]
fn only_native_acp_has_a_memory_provider_root() {
    for runtime in [
        ActorRuntime::Claude,
        ActorRuntime::Codex,
        ActorRuntime::Grok,
        ActorRuntime::Opencode,
        ActorRuntime::Kilo,
        ActorRuntime::Pi,
        ActorRuntime::Antigravity,
    ] {
        assert!(!AnalystSession::supports_memory_restart(
            runtime,
            RuntimeMode::Acp
        ));
    }
    for runtime in [
        ActorRuntime::Copilot,
        ActorRuntime::Devin,
        ActorRuntime::Cursor,
        ActorRuntime::Kimi,
        ActorRuntime::Auggie,
        ActorRuntime::Kiro,
        ActorRuntime::Hermes,
        ActorRuntime::Droid,
    ] {
        assert!(AnalystSession::supports_memory_restart(
            runtime,
            RuntimeMode::Acp
        ));
        assert!(!AnalystSession::supports_memory_restart(
            runtime,
            RuntimeMode::Default
        ));
    }
    let f = Fixture::new(false);
    let provider = f.candidate();
    f.item().status.lock().expect("status").pid = Some(std::process::id());
    assert_eq!(
        f.candidate(),
        provider,
        "projected/viewer PID is never the provider root"
    );
    assert_ne!(provider.provider_pid, std::process::id());
    let item = f.item();
    item.has_terminal
        .store(true, std::sync::atomic::Ordering::Release);
    assert!(
        memory_candidate(&f.group.group_id, "worker").is_none(),
        "a viewer attachment excludes the session even with a native runtime"
    );
    item.has_terminal
        .store(false, std::sync::atomic::Ordering::Release);
}

#[test]
fn memory_restart_loads_the_same_session_and_fences_old_generation() {
    let f = Fixture::new(false);
    f.materialize();
    let before = f.candidate();
    let session_id = f.item().managed.thread_id().to_owned();
    assert_eq!(
        f.restart(&before, |_, _, _| Ok(true)).expect("restart"),
        MemoryRestartOutcome::Restarted
    );
    let after = f.candidate();
    assert_ne!(after.generation, before.generation);
    assert_ne!(after.provider_pid, before.provider_pid);
    assert_eq!(f.item().managed.thread_id(), session_id);
    assert!(
        actor_memory::required_resume(&f.home, &f.group.group_id, "worker")
            .expect("requirement")
            .is_none()
    );
    assert_eq!(
        f.restart(&before, |_, _, _| panic!(
            "stale generation cannot be admitted"
        ))
        .expect("skip"),
        MemoryRestartOutcome::Skipped
    );
    let requests = f.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r["method"] == "session/new")
            .count(),
        1
    );
    let loads: Vec<_> = requests
        .iter()
        .filter(|r| r["method"] == "session/load")
        .collect();
    assert_eq!(loads.len(), 1);
    assert_eq!(loads[0]["params"]["sessionId"], session_id);
}

#[test]
fn closed_runtime_start_gate_preserves_the_live_session() {
    let f = Fixture::new(false);
    f.materialize();
    let before = f.candidate();
    let requests = f.requests().len();
    let admitted = std::cell::Cell::new(false);
    crate::runtime_start_gate::prevent(&f.home).expect("close runtime start gate");
    let result = f.restart(&before, |_, _, _| {
        admitted.set(true);
        Ok(true)
    });
    let after = memory_candidate(&f.group.group_id, "worker");
    crate::runtime_start_gate::allow(&f.home).expect("restore fixture start gate");
    assert_eq!(
        result.expect("closed gate skips"),
        MemoryRestartOutcome::Skipped
    );
    assert!(
        !admitted.get(),
        "shutdown must precede final admission or cooldown writes"
    );
    assert_eq!(
        after.as_ref(),
        Some(&before),
        "live provider must remain untouched"
    );
    assert_eq!(
        f.requests().len(),
        requests,
        "no new native launch or close"
    );
    assert!(
        actor_memory::required_resume(&f.home, &f.group.group_id, "worker")
            .expect("requirement")
            .is_none()
    );
}

#[test]
fn missing_unattempted_or_changed_identity_never_stops_the_provider() {
    let f = Fixture::new(false);
    let before = f.candidate();
    assert_eq!(
        f.restart(&before, |_, _, _| panic!("unattempted receipt"))
            .expect("skip"),
        MemoryRestartOutcome::Skipped
    );
    f.materialize();
    crate::ops::actor_secrets::replace(
        &f.home,
        &f.group.group_id,
        "worker",
        BTreeMap::from([("AUTH_IDENTITY".into(), "changed".into())]),
    )
    .expect("private env drift");
    assert_eq!(
        f.restart(&before, |_, _, _| panic!("identity changed"))
            .expect("skip"),
        MemoryRestartOutcome::Skipped
    );
    std::fs::remove_file(f.receipt()).expect("remove receipt");
    assert_eq!(
        f.restart(&before, |_, _, _| panic!("missing receipt"))
            .expect("skip"),
        MemoryRestartOutcome::Skipped
    );
    assert_eq!(f.candidate(), before);
}

#[test]
fn busy_protocol_refuses_without_interrupting_or_leaving_a_requirement() {
    let f = Fixture::new(false);
    f.materialize();
    let before = f.candidate();
    let item = f.item();
    super::super::block_on_managed(item.managed.start_turn(
        item.managed.generation(),
        "held",
        "HOLD fixture",
    ))
    .expect("held turn");
    assert_eq!(
        f.restart(&before, |_, _, _| Ok(true)).expect("skip"),
        MemoryRestartOutcome::Skipped
    );
    assert_eq!(f.candidate(), before);
    assert!(
        actor_memory::required_resume(&f.home, &f.group.group_id, "worker")
            .expect("requirement")
            .is_none()
    );
    assert!(!f.requests().iter().any(|r| r["method"] == "session/cancel"));
}

#[test]
fn controlled_delivery_defers_without_waiting_for_the_maintenance_gate() {
    let f = Fixture::new(false);
    let guard = supervisor::StartGuard::acquire(&(f.group.group_id.clone(), "worker".into()))
        .expect("maintenance gate");
    let (sender, receiver) = std::sync::mpsc::channel();
    let home = f.home.clone();
    let group = f.group.clone();
    let worker = std::thread::spawn(move || {
        let event = Event::new("chat.message", &group.group_id);
        let outcome = supervisor::submit_batch(
            &home,
            &group,
            &group.actors[0],
            &[event],
            &AtomicBool::new(false),
        );
        sender.send(outcome).expect("outcome");
    });
    let outcome = receiver.recv_timeout(Duration::from_secs(1));
    drop(guard);
    worker.join().expect("delivery worker");
    assert_eq!(
        outcome.expect("delivery must not wait for StartGuard"),
        super::super::BatchSubmission::Deferred
    );
    assert!(!f.requests().iter().any(|r| r["method"] == "session/prompt"));
}

#[test]
fn linked_profile_controls_the_fresh_restart_identity() {
    let f = Fixture::new(true);
    f.materialize();
    let expected_home = f.group.actors[0].env["HOME"].clone();
    GroupStore::new(f.home.clone())
        .expect("store")
        .mutate(&f.group.group_id, |group| {
            group.actors[0].runtime = ActorRuntime::Claude;
            group.actors[0].runtime_mode = RuntimeMode::Default;
            group.actors[0].command = vec!["stale-custom-command".into()];
            group.actors[0]
                .env
                .insert("HOME".into(), "/stale/custom/home".into());
            Ok(())
        })
        .expect("stale stored snapshot");
    crate::ops::actor_secrets::replace(
        &f.home,
        &f.group.group_id,
        "worker",
        BTreeMap::from([("DORMANT_CUSTOM_SECRET".into(), "ignored".into())]),
    )
    .expect("dormant custom secret");
    assert_eq!(
        f.restart(&f.candidate(), |_, actor, _| {
            assert_eq!(actor.runtime, ActorRuntime::Kimi);
            assert_eq!(actor.runtime_mode, RuntimeMode::Acp);
            assert_eq!(actor.env["HOME"], expected_home);
            assert!(!actor.env.contains_key("DORMANT_CUSTOM_SECRET"));
            Ok(true)
        })
        .expect("restart linked Profile"),
        MemoryRestartOutcome::Restarted
    );
}

#[cfg(target_os = "linux")]
#[test]
fn memory_tick_uses_the_persisted_limit_instead_of_the_stale_passed_group() {
    let f = Fixture::new(false);
    f.materialize();
    let before = f.candidate();
    let (rss_kib, counted) = crate::ops::actor_memory_sampling::tree_rss_kib(
        i32::try_from(before.provider_pid).expect("provider PID"),
    );
    assert!(
        counted > 0 && rss_kib > 1024,
        "fixture exceeds stale 1 MiB limit"
    );
    let mut stale = f.group.clone();
    stale.actors[0].memory_limit_mb = 1;
    let store = GroupStore::new(f.home.clone()).expect("store");
    store
        .mutate(&f.group.group_id, |group| {
            group.actors[0].memory_limit_mb = 1_000_000;
            Ok(())
        })
        .expect("persist fresh high limit");
    let requests = f.requests().len();
    assert!(
        actor_memory::tick(&f.home, &stale)
            .expect("tick")
            .is_empty()
    );
    assert_eq!(f.candidate(), before, "fresh high limit prevents restart");
    assert_eq!(f.requests().len(), requests, "no replacement launch");
    assert!(
        !store
            .state_dir(&f.group.group_id)
            .expect("state")
            .join("actor_memory.json")
            .exists(),
        "no cooldown reservation"
    );
    assert!(
        actor_memory::required_resume(&f.home, &f.group.group_id, "worker")
            .expect("requirement")
            .is_none()
    );
}

fn receipt_race_never_creates_a_fresh_session(remove: bool) {
    let f = Fixture::new(false);
    f.materialize();
    let before = f.candidate();
    let original_id = f.item().managed.thread_id().to_owned();
    let original_receipt = std::fs::read(f.receipt()).expect("original receipt");
    let error = f
        .restart(&before, |_, _, _| {
            if remove {
                std::fs::remove_file(f.receipt()).expect("receipt disappears after admission");
            } else {
                let mut receipt: Value = cccc_core::fs::read_json(&f.receipt()).expect("receipt");
                receipt["provider_session_id"] = json!("changed-session-id");
                cccc_core::fs::write_json(&f.receipt(), &receipt).expect("changed receipt");
            }
            Ok(true)
        })
        .expect_err("required receipt changed");
    assert!(actor_memory::is_resume_blocked(&error));
    let required = actor_memory::required_resume(&f.home, &f.group.group_id, "worker")
        .expect("durable requirement")
        .expect("requirement retained");
    assert_eq!(required.session_id, original_id);
    assert!(memory_candidate(&f.group.group_id, "worker").is_none());
    let start = supervisor::start(&f.home, &f.group, &f.group.actors[0])
        .expect_err("ordinary auto-start must honor the durable requirement");
    assert!(actor_memory::is_resume_blocked(&start));
    let start = actor_runtime::apply(&f.home, &f.group, "worker", "actor.start")
        .expect_err("ordinary lifecycle must retain the required session gate");
    assert_eq!(start.code, "runtime_resume_failed");
    let mut changed_runtime = f.group.actors[0].clone();
    changed_runtime.runtime = ActorRuntime::Claude;
    changed_runtime.runtime_mode = RuntimeMode::Default;
    let start = supervisor::start(&f.home, &f.group, &changed_runtime)
        .expect_err("another runtime cannot bypass the requirement");
    assert!(actor_memory::is_resume_blocked(&start));
    let requests = f.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r["method"] == "session/new")
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r["method"] == "session/load")
            .count(),
        0
    );
    std::fs::write(f.receipt(), original_receipt).expect("restore original receipt");
    actor_runtime::apply(&f.home, &f.group, "worker", "actor.start")
        .expect("ordinary recovery loads required session");
    assert_eq!(f.item().managed.thread_id(), original_id);
    assert!(
        actor_memory::required_resume(&f.home, &f.group.group_id, "worker")
            .expect("retired requirement")
            .is_none()
    );
    let requests = f.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r["method"] == "session/new")
            .count(),
        1
    );
    let loads: Vec<_> = requests
        .iter()
        .filter(|r| r["method"] == "session/load")
        .collect();
    assert_eq!(loads.len(), 1);
    assert_eq!(loads[0]["params"]["sessionId"], original_id);
}

#[test]
fn disappearing_receipt_blocks_memory_and_ordinary_fresh_fallback() {
    receipt_race_never_creates_a_fresh_session(true);
}

#[test]
fn changed_receipt_blocks_memory_and_ordinary_fresh_fallback() {
    receipt_race_never_creates_a_fresh_session(false);
}

// Append to local_headless/memory_tests.rs only with the production proposal.
#[test]
fn refused_idle_close_never_invokes_the_attempt_commit_callback() {
    let f = Fixture::new(false);
    f.materialize();
    let before = f.candidate();
    let item = f.item();
    super::super::block_on_managed(item.managed.start_turn(
        item.managed.generation(),
        "held",
        "HOLD fixture",
    ))
    .expect("held prompt");
    assert_eq!(
        restart_for_memory_with_commit(
            &f.home,
            &f.group.group_id,
            "worker",
            &before,
            |_, _, _| Ok(true),
            || panic!("busy refusal is not a restart attempt"),
        )
        .expect("busy skip"),
        MemoryRestartOutcome::Skipped,
    );
    assert_eq!(f.candidate(), before);
}

#[test]
fn failed_attempt_commit_still_stops_the_closed_generation_and_retains_exact_resume() {
    let f = Fixture::new(false);
    f.materialize();
    let before = f.candidate();
    let original = f.item();
    let required_id = original.managed.thread_id().to_owned();
    let error = restart_for_memory_with_commit(
        &f.home,
        &f.group.group_id,
        "worker",
        &before,
        |_, _, _| Ok(true),
        || {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected cooldown write failure",
            ))
        },
    )
    .expect_err("commit must fail");
    assert!(actor_memory::is_resume_blocked(&error));
    assert!(memory_candidate(&f.group.group_id, "worker").is_none());
    assert!(
        !original.running(),
        "closed generation must not remain registered as healthy"
    );
    let required = actor_memory::required_resume(&f.home, &f.group.group_id, "worker")
        .expect("durable requirement")
        .expect("requirement retained");
    assert_eq!(required.generation, before.generation);
    assert_eq!(required.session_id, required_id);
    assert!(!f.requests().iter().any(|r| r["method"] == "session/load"));
    assert_eq!(
        f.requests()
            .iter()
            .filter(|r| r["method"] == "session/new")
            .count(),
        1
    );
}
