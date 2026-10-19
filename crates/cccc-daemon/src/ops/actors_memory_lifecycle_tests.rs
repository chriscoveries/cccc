use super::*;
use crate::ops::actor_memory::{self, RequiredResume};
use cccc_contracts::{DaemonResponse, GroupState};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

const ACTOR: &str = "worker";
const OLD_SESSION: &str = "removed-actor-session";

struct Fixture {
    _temp: tempfile::TempDir,
    home: HomeLayout,
    group_id: String,
    workspace: PathBuf,
    program: PathBuf,
    provider_home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("fixture");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("initialize");
        let store = GroupStore::new(home.clone()).expect("store");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let program = temp.path().join("kimi");
        std::fs::write(
            &program,
            include_str!("codex_voice_analyst/native_acp/candidate_fixture.py"),
        )
        .expect("offline native ACP fixture");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))
            .expect("executable");
        let group = store.create("actor memory lifecycle", "").expect("group");
        let group = group_scope::attach(
            &store,
            &group.group_id,
            cccc_core::scope::detect(&workspace).expect("scope"),
        )
        .expect("attach scope");
        store
            .mutate(&group.group_id, |group| {
                group.running = true;
                group.state = GroupState::Active;
                Ok(())
            })
            .expect("activate group");
        Self {
            provider_home: temp.path().join("provider"),
            _temp: temp,
            home,
            group_id: group.group_id,
            workspace,
            program,
        }
    }

    fn request(&self, op: &str, mut args: Value) -> DaemonResponse {
        args["group_id"] = json!(self.group_id);
        args["actor_id"] = json!(ACTOR);
        args["by"] = json!("user");
        crate::dispatch::dispatch(
            &self.home,
            &DaemonRequest {
                v: 1,
                op: op.into(),
                args: args.as_object().expect("request object").clone(),
            },
        )
    }

    fn add(&self) -> DaemonResponse {
        self.request(
            "actor_add",
            json!({
                "runtime": "kimi",
                "runtime_mode": "acp",
                "command": [self.program],
                "env": {
                    "HOME": self.provider_home,
                    "KIMI_CODE_HOME": self.provider_home.join("kimi"),
                    "ACP_FIXTURE_CLI": "/bin/true"
                }
            }),
        )
    }

    fn add_successfully(&self) {
        let response = self.add();
        assert!(response.ok, "actor_add failed: {:?}", response.error);
    }

    fn receipt(&self, runtime: ActorRuntime) -> PathBuf {
        runtime_session::native_acp::receipt_path(&self.home, &self.group_id, ACTOR, runtime)
            .expect("native receipt")
    }

    fn requirement_path(&self) -> PathBuf {
        GroupStore::new(self.home.clone())
            .expect("store")
            .state_dir(&self.group_id)
            .expect("state directory")
            .join("actor_memory_required")
            .join(format!("{ACTOR}.json"))
    }

    fn seed_failed_incarnation(&self) -> RequiredResume {
        let required = RequiredResume {
            generation: "removed-actor-generation".into(),
            runtime: ActorRuntime::Kimi,
            session_id: OLD_SESSION.into(),
        };
        runtime_session::native_acp::record(
            &self.receipt(ActorRuntime::Kimi),
            &json!({"fixture": "removed actor incarnation"}),
            OLD_SESSION,
            true,
        )
        .expect("old materialized receipt");
        actor_memory::require_resume(&self.home, &self.group_id, ACTOR, &required)
            .expect("failed maintenance requirement");
        required
    }

    fn has_actor(&self) -> bool {
        GroupStore::new(self.home.clone())
            .expect("store")
            .load(&self.group_id)
            .expect("group")
            .actors
            .iter()
            .any(|actor| actor.id == ACTOR)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = crate::ops::local_headless::stop(&self.group_id, ACTOR);
    }
}

#[test]
fn removed_actor_memory_requirement_does_not_follow_a_reused_id() {
    let f = Fixture::new();
    f.add_successfully();
    f.seed_failed_incarnation();

    let removed = f.request("actor_remove", json!({}));
    assert!(removed.ok, "actor_remove failed: {:?}", removed.error);
    assert!(!f.has_actor());
    assert!(!f.receipt(ActorRuntime::Kimi).exists());
    assert!(
        actor_memory::required_resume(&f.home, &f.group_id, ACTOR)
            .expect("requirement after removal")
            .is_none()
    );

    f.add_successfully();
    let started = f.request("actor_start", json!({}));
    assert!(started.ok, "replacement start failed: {:?}", started.error);
    let receipt: Value =
        cccc_core::fs::read_json(&f.receipt(ActorRuntime::Kimi)).expect("replacement receipt");
    assert_ne!(receipt["provider_session_id"], OLD_SESSION);
    let requests: Vec<Value> =
        std::fs::read_to_string(f.workspace.join("candidate_requests.jsonl"))
            .expect("offline provider requests")
            .lines()
            .map(|line| serde_json::from_str(line).expect("request JSON"))
            .collect();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request["method"] == "session/new")
            .count(),
        1,
        "the replacement creates its own first session without actor_new_session"
    );
    assert!(
        requests
            .iter()
            .all(|request| request["method"] != "session/load"),
        "a recreated actor must never resume the removed incarnation"
    );
}

#[test]
fn actor_add_cleans_a_leftover_memory_requirement_with_old_native_receipts() {
    let f = Fixture::new();
    f.seed_failed_incarnation();

    f.add_successfully();

    assert!(f.has_actor());
    assert!(!f.receipt(ActorRuntime::Kimi).exists());
    assert!(
        actor_memory::required_resume(&f.home, &f.group_id, ACTOR)
            .expect("requirement after addition")
            .is_none()
    );
}

#[test]
fn actor_add_keeps_the_memory_fence_if_native_receipt_cleanup_fails() {
    let f = Fixture::new();
    let required = f.seed_failed_incarnation();
    // Copilot is retired before Kimi. An unremovable receipt entry must not
    // clear the fence or leave the newly added actor available for startup.
    let blocked = f.receipt(ActorRuntime::Copilot);
    std::fs::create_dir_all(&blocked).expect("blocked native receipt");

    let added = f.add();

    assert!(!added.ok, "addition must fail closed on receipt cleanup");
    assert!(!f.has_actor(), "failed addition must roll back");
    assert!(f.receipt(ActorRuntime::Kimi).is_file());
    assert_eq!(
        actor_memory::required_resume(&f.home, &f.group_id, ACTOR).expect("retained requirement"),
        Some(required)
    );
}

#[test]
fn actor_add_rolls_back_if_the_old_memory_requirement_cannot_be_removed() {
    let f = Fixture::new();
    f.seed_failed_incarnation();
    let requirement = f.requirement_path();
    std::fs::remove_file(&requirement).expect("replace requirement");
    std::fs::create_dir(&requirement).expect("unremovable requirement entry");

    let added = f.add();

    assert!(!added.ok, "addition must fail closed on fence cleanup");
    assert!(!f.has_actor(), "failed addition must roll back");
    assert!(requirement.is_dir());
    assert!(!f.receipt(ActorRuntime::Kimi).exists());
    assert!(!f.workspace.join("candidate_requests.jsonl").exists());
}

#[test]
fn failed_actor_removal_preserves_the_memory_requirement_and_native_receipt() {
    let f = Fixture::new();
    f.add_successfully();
    let required = f.seed_failed_incarnation();
    let receipt = std::fs::read(f.receipt(ActorRuntime::Kimi)).expect("old receipt");
    let ledger = GroupStore::new(f.home.clone())
        .expect("store")
        .ledger_path(&f.group_id)
        .expect("ledger path");
    std::fs::remove_file(&ledger).expect("replace ledger");
    std::fs::create_dir(&ledger).expect("blocked removal commit");

    let removed = f.request("actor_remove", json!({}));

    assert!(
        !removed.ok,
        "removal must fail when its event cannot commit"
    );
    assert!(f.has_actor(), "failed removal must restore the actor");
    assert_eq!(
        actor_memory::required_resume(&f.home, &f.group_id, ACTOR).expect("preserved requirement"),
        Some(required)
    );
    assert_eq!(
        std::fs::read(f.receipt(ActorRuntime::Kimi)).expect("preserved native receipt"),
        receipt,
        "receipt cleanup must follow the removal commit"
    );
}
