//! Install as crates/cccc-daemon/tests/memory_policy_red.rs.
//! Exercises real actor_update + GroupStore persistence, without starting providers.
use cccc_contracts::{Actor, DaemonRequest, GroupState};
use cccc_core::{GroupStore, HomeLayout};
use serde_json::{Value, json};

#[test]
fn actor_update_preserves_explicit_off_and_override_across_unrelated_updates() {
    let temp = tempfile::tempdir().expect("private home");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    let store = GroupStore::new(home.clone()).expect("store");
    let mut group = store
        .create("memory policy persistence", "")
        .expect("group");
    group.running = false;
    group.state = GroupState::Stopped;
    group.actors.push(Actor::new("worker"));
    store.save(&group).expect("save actor");
    for limit in [0_u32, 1536] {
        for patch in [
            json!({"memory_limit_mb":limit}),
            json!({"title":"retained limit"}),
        ] {
            let request = DaemonRequest {
                v: 1,
                op: "actor_update".into(),
                args:
                    json!({"group_id":group.group_id,"actor_id":"worker","by":"user","patch":patch})
                        .as_object()
                        .expect("args")
                        .clone(),
            };
            let response = cccc_daemon::handle_request(&home, &request);
            assert!(response.ok, "actor_update failed: {response:?}");
            let loaded = store.load(&group.group_id).expect("reload group");
            let saved: Value = serde_json::to_value(&loaded.actors[0]).expect("serialized actor");
            assert_eq!(saved.get("memory_limit_mb"), Some(&json!(limit)), "{saved}");
        }
    }
}
