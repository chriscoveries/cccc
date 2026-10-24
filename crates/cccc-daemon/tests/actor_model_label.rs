use cccc_contracts::{Actor, DaemonRequest, DaemonResponse};
use cccc_core::{GroupStore, HomeLayout, ledger};
use serde_json::{Value, json};

const ACTOR: &str = include_str!("../../../tests/fixtures/actor-model-label/actor.json");
const LABELS: &str = include_str!("../../../tests/fixtures/actor-model-label/labels.json");

fn request(home: &HomeLayout, op: &str, args: Value) -> DaemonResponse {
    cccc_daemon::handle_request(
        home,
        &DaemonRequest {
            v: 1,
            op: op.into(),
            args: args.as_object().expect("args").clone(),
        },
    )
}

fn call(home: &HomeLayout, op: &str, args: Value) -> DaemonResponse {
    let response = request(home, op, args);
    assert!(response.ok, "{op}: {:?}", response.error);
    response
}

fn row(home: &HomeLayout, group_id: &str) -> Value {
    call(home, "actor_list", json!({"group_id":group_id})).result["actors"][0].clone()
}

fn update(home: &HomeLayout, group_id: &str, patch: Value) -> DaemonResponse {
    request(
        home,
        "actor_update",
        json!({"group_id":group_id,"actor_id":"worker","patch":patch,"by":"user"}),
    )
}

#[test]
fn legacy_actor_is_undeclared_and_model_is_not_an_alias() {
    let mut captured: Value = serde_json::from_str(ACTOR).expect("captured actor");
    let labels: Vec<String> = serde_json::from_str(LABELS).expect("captured tokens");
    for legacy_key in [false, true] {
        if legacy_key {
            captured["model"] = json!(labels[0]);
        }
        let actor: Actor = serde_json::from_value(captured.clone()).expect("legacy actor");
        let serialized = serde_json::to_value(actor).expect("actor");
        assert!(serialized.get("model_label").is_none());
        assert!(serialized.get("model").is_none());
    }
}

#[test]
fn actor_model_label_persists_validates_and_clears_without_launch_changes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home_path = temp.path().join("home");
    let home = HomeLayout::from_path(&home_path).expect("home");
    let created = call(&home, "group_create", json!({}));
    let group_id = created.result["group_id"].as_str().expect("group id");
    let labels: Vec<String> = serde_json::from_str(LABELS).expect("captured tokens");
    let mut actor: Value = serde_json::from_str(ACTOR).expect("captured actor");
    // These captured read fields are derived, and cannot be selected on add.
    actor.as_object_mut().expect("actor").remove("runner");
    actor
        .as_object_mut()
        .expect("actor")
        .remove("runtime_state_source");
    actor["model_label"] = json!(format!("  {}  ", labels[0]));
    let added = call(
        &home,
        "actor_add",
        json!({
            "group_id":group_id,"actor_id":actor["id"],"runtime":actor["runtime"],
            "command":actor["command"],"submit":actor["submit"],"enabled":actor["enabled"],
            "model_label":actor["model_label"],"by":"user"
        }),
    );
    assert_eq!(added.result["actor"]["model_label"], labels[0]);
    assert_eq!(
        added.result["event"]["data"]["actor"]["model_label"],
        labels[0]
    );
    let baseline = added.result["actor"].clone();
    let initial_row = row(&home, group_id);
    assert_eq!(initial_row["model_label"], labels[0]);
    assert_eq!(initial_row["running"], false);

    // Reconstruct the layout/store to read the persisted document, not the response.
    let reopened = HomeLayout::from_path(&home_path).expect("reopened home");
    let store = GroupStore::new(reopened.clone()).expect("fresh store");
    let persisted = store.load(group_id).expect("persisted group");
    assert_eq!(
        serde_json::to_value(&persisted.actors[0]).expect("actor")["model_label"],
        labels[0]
    );
    assert!(update(&reopened, group_id, json!({})).ok);
    assert_eq!(row(&reopened, group_id)["model_label"], labels[0]);
    assert!(update(&reopened, group_id, json!({"model_label":labels[1]})).ok);
    assert_eq!(row(&reopened, group_id)["model_label"], labels[1]);

    let boundary: String = labels[0].chars().cycle().take(128).collect();
    assert!(update(&reopened, group_id, json!({"model_label":boundary})).ok);
    assert_eq!(row(&reopened, group_id)["model_label"], boundary);
    assert!(update(&reopened, group_id, json!({"model_label":labels[1]})).ok);
    let validation_group = call(&home, "group_create", json!({}));
    let validation_group_id = validation_group.result["group_id"]
        .as_str()
        .expect("group id");
    // Controlled invalid mutations of the captured token, not captured observations.
    for invalid in [
        json!(format!(
            "{boundary}{}",
            labels[0].chars().next().expect("token")
        )),
        json!(format!("{}\n{}", labels[0], labels[1])),
        json!(format!("\t{}", labels[0])),
        json!(format!("{}\u{7f}", labels[0])),
        Value::Null,
        json!(128),
    ] {
        let before = row(&reopened, group_id);
        let events_before = ledger::read_all(&store.ledger_path(group_id).expect("ledger"))
            .expect("events")
            .len();
        assert!(!update(&reopened, group_id, json!({"model_label":invalid})).ok);
        assert_eq!(row(&reopened, group_id), before);
        assert_eq!(
            ledger::read_all(&store.ledger_path(group_id).expect("ledger"))
                .expect("events")
                .len(),
            events_before
        );
        let mut invalid_actor = actor.clone();
        invalid_actor["model_label"] = invalid;
        assert!(
            !request(
                &reopened,
                "actor_add",
                json!({"group_id":validation_group_id,"actor":invalid_actor,"by":"user"}),
            )
            .ok
        );
        assert_eq!(
            call(
                &reopened,
                "actor_list",
                json!({"group_id":validation_group_id})
            )
            .result["actors"],
            json!([])
        );
    }

    // The unmerged key is ignored like other unknown Actor fields, never aliased.
    assert!(update(&reopened, group_id, json!({"model":labels[0]})).ok);
    assert_eq!(row(&reopened, group_id)["model_label"], labels[1]);
    for clear in ["", "   "] {
        assert!(update(&reopened, group_id, json!({"model_label":clear})).ok);
        assert!(row(&reopened, group_id).get("model_label").is_none());
        let persisted = store.load(group_id).expect("persisted clear");
        let mut current = serde_json::to_value(&persisted.actors[0]).expect("actor");
        assert!(current.get("model_label").is_none());
        let mut expected = baseline.clone();
        for value in [&mut current, &mut expected] {
            let object = value.as_object_mut().expect("actor object");
            for key in ["model_label", "updated_at", "role"] {
                object.remove(key);
            }
        }
        assert_eq!(
            current, expected,
            "launch configuration and identity are stable"
        );
        let current_row = row(&reopened, group_id);
        for key in ["running", "pid", "runtime_state_source"] {
            assert_eq!(current_row[key], initial_row[key]);
        }
        assert!(update(&reopened, group_id, json!({"model_label":labels[0]})).ok);
    }
    let events = ledger::read_all(&store.ledger_path(group_id).expect("ledger")).expect("events");
    assert!(!events.iter().any(|event| matches!(
        event.kind.as_str(),
        "actor.start" | "actor.stop" | "actor.restart"
    )));
}
