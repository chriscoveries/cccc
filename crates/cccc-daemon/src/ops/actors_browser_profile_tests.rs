use super::*;

struct Fixture {
    _temp: tempfile::TempDir,
    home: HomeLayout,
    group: GroupDoc,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temporary home");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        home.initialize().expect("initialize");
        // Configuration controls over the retained metadata capture. No provider
        // conversation or account is fabricated or contacted by these tests.
        let mut group: GroupDoc = serde_json::from_str(include_str!(
            "../../../cccc-core/tests/fixtures/browser-profile-group.json"
        ))
        .expect("retained capture");
        group.running = false;
        let actor = group
            .actors
            .iter_mut()
            .find(|a| a.id == "web1")
            .expect("actor");
        actor.enabled = false;
        actor.profile_id.clear();
        let group = GroupStore::new(home.clone())
            .expect("store")
            .import(group)
            .expect("import");
        Self {
            _temp: temp,
            home,
            group,
        }
    }

    fn actor(&self) -> Actor {
        GroupStore::new(self.home.clone())
            .expect("store")
            .load(&self.group.group_id)
            .expect("group")
            .actors
            .into_iter()
            .find(|a| a.id == "web1")
            .expect("actor")
    }

    fn request(&self, op: &str, extra: Value) -> cccc_contracts::DaemonResponse {
        let mut args = json!({"by":"user","group_id":self.group.group_id,"actor_id":"web1",
            "expected_browser_identity":actors::web_model_generation_identity(&self.actor())})
        .as_object()
        .expect("arguments")
        .clone();
        args.extend(extra.as_object().expect("control arguments").clone());
        crate::handle_request(
            &self.home,
            &DaemonRequest {
                v: 1,
                op: op.into(),
                args,
            },
        )
    }

    fn edit(&self) -> cccc_contracts::DaemonResponse {
        self.request("actor_update", json!({"browser_profile":"secondary"}))
    }
}

#[test]
fn stopped_unpaired_edit_keeps_history_and_fences_stale_and_spoofed_revisions() {
    let fixture = Fixture::new();
    let groups = GroupStore::new(fixture.home.clone()).expect("store");
    let mut group = groups.load(&fixture.group.group_id).expect("group");
    let old_identity = actors::web_model_generation_identity(&fixture.actor());
    let old_target = group.extra[WEB_MODEL_TARGETS_KEY]["web1"].clone();
    group.extra.get_mut(WEB_MODEL_TARGETS_KEY).expect("targets")["web1"]["setup_url"] =
        old_target["url"].clone();
    groups.save(&group).expect("setup destination control");
    let response = fixture.edit();
    assert!(response.ok, "{response:?}");
    assert_eq!(fixture.actor().browser_profile, "secondary");
    let saved = groups.load(&group.group_id).expect("saved");
    assert_eq!(saved.extra[WEB_MODEL_TARGETS_KEY]["web1"], old_target);
    let stale = fixture.request(
        "actor_update",
        json!({"browser_profile":"_shared","expected_browser_identity":old_identity}),
    );
    assert_eq!(
        stale.error.expect("stale selection refused").code,
        "browser_profile_changed"
    );
    let spoof = fixture.request(
        "actor_update",
        json!({"patch":{"browser_profile_revision":0}}),
    );
    assert!(!spoof.ok);
    assert_eq!(fixture.actor().browser_profile_revision, 1);
}

#[test]
fn enabled_and_held_browser_work_refuse_profile_changes_without_state_changes() {
    let fixture = Fixture::new();
    let groups = GroupStore::new(fixture.home.clone()).expect("store");
    let mut enabled = groups.load(&fixture.group.group_id).expect("group");
    enabled
        .actors
        .iter_mut()
        .find(|a| a.id == "web1")
        .expect("actor")
        .enabled = true;
    groups.save(&enabled).expect("enabled control");
    assert_eq!(
        fixture.edit().error.expect("enabled refused").code,
        "browser_profile_busy"
    );
    groups
        .save(&fixture.group)
        .expect("restore stopped capture");
    let _work = actor_runtime::hold_browser_work(&fixture.home, &fixture.group.group_id, "web1")
        .expect("hold work");
    assert_eq!(
        fixture.edit().error.expect("in-flight work refused").code,
        "actor_browser_busy"
    );
    assert_eq!(fixture.actor().browser_profile_revision, 0);
}

#[test]
fn pending_pairing_and_bound_conversation_refuse_profile_changes() {
    let fixture = Fixture::new();
    let configured = fixture.request("web_model_connector_configure", json!({}));
    assert!(configured.ok, "{configured:?}");
    let id = configured.result["connector"]["connector_id"].clone();
    let pair = fixture.request("web_model_pairing_begin", json!({"connector_id":id}));
    assert!(pair.ok, "{pair:?}");
    assert_eq!(
        fixture.edit().error.expect("pending pairing refused").code,
        "browser_profile_busy"
    );
    let accepted = fixture.request(
        "web_model_pairing_accept",
        json!({"connector_id":id,
        "code":pair.result["code"],"session_key":"profile-test"}),
    );
    assert!(accepted.ok, "{accepted:?}");
    let bound = fixture.request("web_model_pairing_confirm", json!({"connector_id":id,
        "pairing_id":pair.result["pairing_id"],"url":fixture.group.extra[WEB_MODEL_TARGETS_KEY]["web1"]["url"]}));
    assert!(bound.ok, "{bound:?}");
    assert_eq!(
        fixture
            .edit()
            .error
            .expect("bound conversation refused")
            .code,
        "browser_profile_busy"
    );
    assert_eq!(fixture.actor().browser_profile_revision, 0);
    let disconnected = fixture.request("web_model_binding_remove", json!({"connector_id":id}));
    assert!(disconnected.ok, "{disconnected:?}");
    assert!(fixture.edit().ok);
    let late = fixture.request("web_model_pairing_confirm", json!({"connector_id":id,
        "pairing_id":pair.result["pairing_id"],"url":fixture.group.extra[WEB_MODEL_TARGETS_KEY]["web1"]["url"]}));
    assert!(!late.ok, "an old callback cannot bind the new profile");
}

#[test]
fn ambiguous_delivery_control_refuses_but_completed_history_alone_allows() {
    for status in [
        "submitting",
        "submission_ambiguous",
        "completion_ambiguous",
        "pending_new_chat_bind",
        "submitted",
    ] {
        let fixture = Fixture::new();
        let groups = GroupStore::new(fixture.home.clone()).expect("store");
        let mut group = groups.load(&fixture.group.group_id).expect("group");
        group.extra.get_mut(WEB_MODEL_TARGETS_KEY).expect("targets")["web1"]["last_delivery_status"] =
            json!(status);
        groups.save(&group).expect("delivery-state control");
        let response = fixture.edit();
        assert_eq!(response.ok, status == "submitted", "{status}: {response:?}");
        assert_eq!(
            groups.load(&group.group_id).expect("saved").extra[WEB_MODEL_TARGETS_KEY],
            group.extra[WEB_MODEL_TARGETS_KEY]
        );
    }
}

#[test]
fn non_admin_and_actor_origin_cannot_select_a_profile() {
    for auth in [
        json!({"is_admin":false}),
        json!({"by":"peer1","is_admin":true}),
        json!({"by":"system","is_admin":true}),
    ] {
        let fixture = Fixture::new();
        let mut args = auth.as_object().expect("authority control").clone();
        args.insert("browser_profile".into(), json!("secondary"));
        assert!(!fixture.request("actor_update", Value::Object(args)).ok);
        assert_eq!(fixture.actor().browser_profile_revision, 0);
    }
}
