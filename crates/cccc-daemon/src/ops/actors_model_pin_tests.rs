use super::*;

fn captured_actor_value() -> Value {
    serde_json::from_str(include_str!(
        "../../../cccc-core/tests/fixtures/web-model-pin/actor.json"
    ))
    .expect("redacted real Actor metadata")
}

fn fixture() -> (tempfile::TempDir, HomeLayout, GroupDoc) {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    let store = GroupStore::new(home.clone()).expect("store");
    let mut group = store.create("", "").expect("test storage envelope");
    let actor = serde_json::from_value(captured_actor_value()).expect("captured Actor");
    actors::add(&mut group, actor).expect("Actor add");
    store.save(&group).expect("save");
    (temp, home, group)
}

fn request(group: &GroupDoc, patch: Value) -> DaemonRequest {
    DaemonRequest {
        v: 1,
        op: "actor_update".into(),
        args: json!({
            "group_id":group.group_id, "actor_id":"browser", "by":"user", "patch":patch
        })
        .as_object()
        .expect("args")
        .clone(),
    }
}

#[test]
fn creation_rejects_explicit_revision_in_flat_and_nested_arguments() {
    let mut actor = captured_actor_value();
    // Derived output fields are not accepted as creation input.
    actor.as_object_mut().expect("Actor").remove("runner");
    actor["web_model_pin_revision"] = json!(0); // Forged field mutation.
    for args in [
        json!({"actor":actor}),
        json!({"actor_id":"browser","runtime":"web_model","web_model_pin_revision":0}),
    ] {
        let request = DaemonRequest {
            v: 1,
            op: "actor_add".into(),
            args: args.as_object().expect("args").clone(),
        };
        let error = actor_from_args(&request).expect_err("server-owned revision");
        assert_eq!(error.message, "web_model_pin_revision is server-owned");
    }
}

#[test]
fn policy_save_refuses_while_browser_work_lock_is_held() {
    let (_temp, home, group) = fixture();
    let before = store(&home)
        .expect("store")
        .load(&group.group_id)
        .expect("before");
    let _work = actor_runtime::hold_browser_work(&home, &group.group_id, "browser")
        .expect("existing browser-work permit");
    let error = update(
        &home,
        &request(
            &group,
            json!({"web_model_pin":{"model":"gpt-6","effort":"extra_high"}}),
        ),
    )
    .expect_err("must not edit during browser work");
    assert_eq!(error.code, "actor_browser_busy");
    assert_eq!(
        store(&home)
            .expect("store")
            .load(&group.group_id)
            .expect("after"),
        before
    );
}

#[test]
fn active_turn_refuses_pin_edit_without_settling_or_retiring_work() {
    let (_temp, home, group) = fixture();
    let store = store(&home).expect("store");
    // Explicit active-turn fault/control, not a captured delivery receipt.
    cccc_core::integration_state::group_update(&store, &group.group_id, RUNTIME_STATES_KEY, |value| {
        *value = json!({"browser":{"status":"working","active_turn_id":"unfinished-control","active_event_ids":[]}});
        Ok(())
    }).expect("runtime boundary control");
    let before = store.load(&group.group_id).expect("before");
    let runtime_before =
        super::super::runtime_state::actor_state(&home, &group.group_id, "browser")
            .expect("runtime before");
    let error = update(
        &home,
        &request(
            &group,
            json!({"web_model_pin":{"model":"gpt-6","effort":"extra_high"}}),
        ),
    )
    .expect_err("active turn remains authoritative");
    assert_eq!(error.code, "delivery_unresolved");
    assert_eq!(store.load(&group.group_id).expect("after"), before);
    assert_eq!(
        super::super::runtime_state::actor_state(&home, &group.group_id, "browser")
            .expect("runtime after"),
        runtime_before
    );
}
