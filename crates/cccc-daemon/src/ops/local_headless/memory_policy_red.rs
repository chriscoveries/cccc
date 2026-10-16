//! Install as ops/local_headless/memory_policy_red.rs, nested in supervisor.
//! Deliberately uses no actor_memory/memory_candidate/required_resume API:
//! every name exists in both 953ad1fa and 4583e5fa. Calls the real scheduler.
use super::*;
use crate::ops::{actor_delivery, actor_runtime, automation_runtime};
use cccc_contracts::{GroupState, RuntimeMode};
use cccc_core::GroupStore;
use serde_json::Value;
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

struct PolicyFixture {
    _temp: tempfile::TempDir,
    home: HomeLayout,
    group: GroupDoc,
    workspace: PathBuf,
}

impl PolicyFixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("private fixture");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let program = temp.path().join("kimi");
        std::fs::write(
            &program,
            include_str!("../codex_voice_analyst/native_acp/candidate_fixture.py"),
        )
        .expect("existing offline ACP fixture");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))
            .expect("executable fixture");
        let mut group = store.create("memory scheduler canary", "").expect("group");
        group = cccc_core::group_scope::attach(
            &store,
            &group.group_id,
            cccc_core::scope::detect(&workspace).expect("scope"),
        )
        .expect("attach scope");
        // A separate offline foreman receives the actual scheduler dispatch.
        // Its explicit zero limit and private environment do not affect workers.
        let mut lead = Actor::new("lead");
        lead.runtime = ActorRuntime::Custom;
        lead.command = vec![
            "python3".into(),
            "-u".into(),
            "-c".into(),
            "import sys; print('offline foreman ready',flush=True); [None for line in sys.stdin]"
                .into(),
        ];
        lead.env.insert(
            "HOME".into(),
            temp.path()
                .join("foreman-home")
                .to_string_lossy()
                .into_owned(),
        );
        let mut lead_json = serde_json::to_value(lead).expect("foreman JSON");
        lead_json["memory_limit_mb"] = json!(0);
        let mut lead: Actor = serde_json::from_value(lead_json).expect("unlimited foreman");
        lead.normalize_runtime_constraints();
        let mut worker = Actor::new("worker");
        worker.runtime = ActorRuntime::Kimi;
        worker.runtime_mode = RuntimeMode::Acp;
        worker.command = vec![program.to_string_lossy().into_owned()];
        worker.env = BTreeMap::from([
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
        let mut worker_json = serde_json::to_value(worker).expect("worker JSON");
        worker_json["memory_limit_mb"] = json!(1);
        let mut worker: Actor = serde_json::from_value(worker_json).expect("1 MiB worker");
        worker.normalize_runtime_constraints();
        group.actors = vec![lead, worker];
        group.running = true;
        group.state = GroupState::Active;
        store.save(&group).expect("save actors");
        let f = Self {
            _temp: temp,
            home,
            group,
            workspace,
        };
        // Construct the cleanup owner before either provider is started.
        actor_runtime::apply(&f.home, &f.group, "lead", "actor.start")
            .expect("start offline foreman");
        actor_runtime::apply(&f.home, &f.group, "worker", "actor.start")
            .expect("start offline worker");
        f.materialize();
        f
    }

    fn item(&self) -> Arc<Session> {
        lookup(&(self.group.group_id.clone(), "worker".into())).expect("worker session")
    }

    fn identity(&self) -> (String, u32, String) {
        let item = self.item();
        (
            item.managed.generation().to_owned(),
            item.managed
                .process_id()
                .expect("directly owned fixture provider"),
            item.managed.thread_id().to_owned(),
        )
    }

    fn materialize(&self) {
        let item = self.item();
        crate::ops::local_headless::block_on_managed(async {
            let mut events = item.managed.subscribe();
            item.managed
                .start_turn(item.managed.generation(), "materialize", "fixture")
                .await
                .expect("fixture prompt");
            tokio::time::timeout(Duration::from_secs(5), async {
                while events.recv().await.expect("event").message["method"] != "turn/completed" {}
            })
            .await
            .expect("fixture completion");
        });
        let until = Instant::now() + Duration::from_secs(5);
        while !ready_for_mail_notice(&self.group.group_id, "worker") {
            assert!(Instant::now() < until, "fixture must project idle");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(self.workspace.join("candidate_requests.jsonl"))
            .expect("requests")
            .lines()
            .map(|line| serde_json::from_str(line).expect("request JSON"))
            .collect()
    }

    fn notices(&self) -> Vec<Event> {
        let store = GroupStore::new(self.home.clone()).expect("store");
        cccc_core::ledger::read_all(&store.ledger_path(&self.group.group_id).expect("ledger"))
            .expect("events")
            .into_iter()
            .filter(|event| event.kind == "system.notify" && event.data["kind"] == "memory_restart")
            .collect()
    }

    fn cooldown_path(&self) -> PathBuf {
        GroupStore::new(self.home.clone())
            .expect("store")
            .state_dir(&self.group.group_id)
            .expect("state")
            .join("actor_memory.json")
    }

    fn required_resume_path(&self) -> PathBuf {
        GroupStore::new(self.home.clone())
            .expect("store")
            .state_dir(&self.group.group_id)
            .expect("state")
            .join("actor_memory_required/worker.json")
    }

    fn tick(&self) {
        automation_runtime::tick_group(
            &self.home,
            &self.group.group_id,
            true,
            &AtomicBool::new(false),
        );
    }

    #[cfg(target_os = "linux")]
    fn assert_idle_over_limit(&self) {
        assert!(self.item().running());
        assert!(
            ready_for_mail_notice(&self.group.group_id, "worker"),
            "second tick must not pass merely because the worker is busy"
        );
        let pid = self.identity().1;
        let status =
            std::fs::read_to_string(format!("/proc/{pid}/status")).expect("live fixture status");
        let rss: u64 = status
            .lines()
            .find_map(|line| {
                line.strip_prefix("VmRSS:")
                    .and_then(|rest| rest.split_whitespace().next())
                    .and_then(|number| number.parse().ok())
            })
            .expect("fixture RSS");
        assert!(
            rss > 1024,
            "real fixture RSS must exceed its 1 MiB override"
        );
    }
}

impl Drop for PolicyFixture {
    fn drop(&mut self) {
        // Cancel/join owned delivery workers before stopping only this fixture's actors.
        actor_delivery::shutdown_group(&self.group.group_id);
        for actor in ["worker", "lead"] {
            let _ = actor_runtime::apply(&self.home, &self.group, actor, "actor.stop");
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn over_limit_scheduler_restart_resumes_notifies_and_then_honors_cooldown() {
    let f = PolicyFixture::new();
    f.assert_idle_over_limit();
    let before = f.identity();
    f.tick();
    let after = f.identity();
    assert_ne!(
        after.0, before.0,
        "actual scheduler must replace the over-limit provider generation"
    );
    assert_ne!(after.1, before.1, "actual provider PID must change");
    assert_eq!(
        after.2, before.2,
        "replacement must load the exact materialized session"
    );
    let notices = f.notices();
    assert_eq!(notices.len(), 1, "one durable restart notice");
    assert_eq!(notices[0].data["context"]["outcome"], "restarted");
    assert_eq!(notices[0].data["context"]["actor_id"], "worker");
    assert_eq!(notices[0].data["target_actor_id"], "lead");
    assert_eq!(notices[0].data["to"], json!(["@foreman"]));
    assert!(!cccc_core::inbox::is_for_actor(
        &f.group,
        &notices[0],
        "worker"
    ));
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
    assert_eq!(loads[0]["params"]["sessionId"], before.2);
    assert!(
        !f.required_resume_path().exists(),
        "verified attachment retires the exact-resume requirement"
    );
    let mark = std::fs::read(f.cooldown_path()).expect("durable cooldown after real restart");
    let state: Value = serde_json::from_slice(&mark).expect("cooldown JSON");
    assert!(state["restarts"]["worker"].as_u64().is_some());
    f.assert_idle_over_limit();
    f.tick();
    assert_eq!(
        f.identity(),
        after,
        "cooldown alone prevents a second eligible restart"
    );
    assert_eq!(f.notices().len(), 1, "cooldown appends no duplicate notice");
    assert_eq!(
        std::fs::read(f.cooldown_path()).expect("same cooldown"),
        mark
    );
    assert_eq!(
        f.requests()
            .iter()
            .filter(|r| r["method"] == "session/load")
            .count(),
        1
    );
}

#[cfg(target_os = "linux")]
#[test]
fn authoritative_busy_refusal_does_not_consume_cooldown_despite_stale_idle_projection() {
    let f = PolicyFixture::new();
    let item = f.item();
    crate::ops::local_headless::block_on_managed(item.managed.start_turn(
        item.managed.generation(),
        "held",
        "HOLD fixture",
    ))
    .expect("hold an actual ACP prompt");
    let until = Instant::now() + Duration::from_secs(5);
    while item.active_turn.lock().expect("turn projection").is_none()
        || item.status.lock().expect("status projection").status != "working"
    {
        assert!(
            Instant::now() < until,
            "reader must observe held turn before stale projection is injected"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // Model the documented projection-vs-authority race. The actual ACP loop
    // and provider remain busy; only the cheap negative filter is stale.
    *item.active_turn.lock().expect("stale turn projection") = None;
    item.status.lock().expect("stale idle projection").status = "idle".into();
    f.assert_idle_over_limit();
    let before = f.identity();
    f.tick();
    assert_eq!(
        f.identity(),
        before,
        "busy provider must remain owned and running"
    );
    assert!(
        f.notices().is_empty(),
        "no restart attempt means no restart outcome"
    );
    assert!(
        !f.cooldown_path().exists(),
        "refused idle-close must not suppress enforcement for 30 minutes"
    );
    assert!(
        !f.required_resume_path().exists(),
        "refused close leaves no maintenance sidecar"
    );
    let requests = f.requests();
    assert!(!requests.iter().any(|r| r["method"] == "session/cancel"));
    assert!(!requests.iter().any(|r| r["method"] == "session/load"));
}
