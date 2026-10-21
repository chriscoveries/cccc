use cccc_contracts::{DaemonRequest, DaemonResponse};
use cccc_core::{GroupStore, HomeLayout, ledger};
use serde_json::{Map, Value, json};

/// A Foreman removed itself on 2026-10-21 09:44:26Z and the role fell to the
/// next Actor for about ninety minutes: sends to the old Foreman began failing
/// `invalid_recipient` and nothing said the role had moved. No Actor may do
/// that to itself or to the Actor currently leading, and when the user does
/// move the role the ledger records the succession.
#[test]
fn no_actor_removes_the_foreman_and_the_user_succession_is_recorded() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("rust-home")).expect("home");
    let group_id = create_group(&home);
    for actor in ["lead", "peer1", "peer2"] {
        add_actor(&home, &group_id, actor);
    }

    let self_removal = raw_call(
        &home,
        "actor_remove",
        json!({"group_id":group_id,"actor_id":"lead","by":"lead"}),
    );
    assert!(!self_removal.ok, "the foreman removed itself");
    assert!(
        self_removal
            .error
            .as_ref()
            .is_some_and(|error| error.message.contains("may not remove the foreman")),
        "{:?}",
        self_removal.error
    );

    let peer_axes_foreman = raw_call(
        &home,
        "actor_remove",
        json!({"group_id":group_id,"actor_id":"lead","by":"peer1"}),
    );
    assert!(!peer_axes_foreman.ok, "a peer removed the foreman");

    let peer_leaves = raw_call(
        &home,
        "actor_remove",
        json!({"group_id":group_id,"actor_id":"peer1","by":"peer1"}),
    );
    assert!(!peer_leaves.ok, "a peer removed itself");

    // The Foreman still administers its peers, and that is not a succession.
    let peer_removed = raw_call(
        &home,
        "actor_remove",
        json!({"group_id":group_id,"actor_id":"peer2","by":"lead"}),
    );
    assert!(peer_removed.ok, "{:?}", peer_removed.error);

    let moved = raw_call(
        &home,
        "actor_remove",
        json!({"group_id":group_id,"actor_id":"lead","by":"user"}),
    );
    assert!(moved.ok, "{:?}", moved.error);

    let events = read_ledger(&home, &group_id);
    let successions: Vec<&cccc_contracts::Event> = events
        .iter()
        .filter(|event| event.kind == "group.foreman_change")
        .collect();
    assert_eq!(successions.len(), 1, "one succession, one record");
    assert_eq!(
        successions[0].data.get("from").and_then(Value::as_str),
        Some("lead")
    );
    assert_eq!(
        successions[0].data.get("to").and_then(Value::as_str),
        Some("peer1"),
        "the next visible Actor holds the role after the removal"
    );
    assert_eq!(successions[0].by, "user");
}

fn read_ledger(home: &HomeLayout, group_id: &str) -> Vec<cccc_contracts::Event> {
    let store = GroupStore::new(home.clone()).expect("store");
    let path = store.ledger_path(group_id).expect("ledger path");
    ledger::read_all(&path).expect("ledger")
}

fn create_group(home: &HomeLayout) -> String {
    call(home, "group_create", json!({"title":"foreman removal","by":"user"})).result["group"]
        ["group_id"]
        .as_str()
        .expect("group id")
        .to_owned()
}

fn add_actor(home: &HomeLayout, group_id: &str, actor_id: &str) {
    let response = call(
        home,
        "actor_add",
        json!({"group_id":group_id,"actor_id":actor_id,"by":"user"}),
    );
    assert!(response.ok, "{:?}", response.error);
}

fn call(home: &HomeLayout, op: &str, args: Value) -> DaemonResponse {
    raw_call(home, op, args)
}

fn raw_call(home: &HomeLayout, op: &str, args: Value) -> DaemonResponse {
    cccc_daemon::handle_request(
        home,
        &DaemonRequest {
            v: 1,
            op: op.into(),
            args: args.as_object().cloned().unwrap_or_else(Map::new),
        },
    )
}
