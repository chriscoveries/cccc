use super::*;

fn fixture() -> (tempfile::TempDir, HomeLayout, Value) {
    let temp = tempfile::tempdir().expect("temp");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    home.initialize().expect("initialize");
    let created = configure(&home,"g_test","hark","generation",false).expect("configure");
    (temp,home,created)
}

#[test]
fn credential_is_new_kind_actor_bound_and_shown_once() {
    let (_temp,home,created) = fixture();
    let c = &created["connector"];
    assert_eq!(c["kind"],"hark_connector");
    assert_eq!(c["bindings"].as_object().expect("bindings").len(),1);
    assert_eq!(resolve(c,"","cccc_runtime_wait_next_turn").expect("claim")["actor_id"],"hark");
    assert!(resolve(c,"","cccc_message_send").is_none());
    assert!(resolve(c,"wrong","cccc_runtime_wait_next_turn").is_none());
    assert!(configure(&home,"g_test","hark","generation",false).is_err());
    let disk = std::fs::read_to_string(store_path(&home)).expect("store");
    assert!(!disk.contains(created["secret"].as_str().expect("secret")));
    let shared = super::super::configure(&home).expect("shared");
    assert!(resolve(&shared["connector"],"","cccc_runtime_wait_next_turn").is_none());
}

#[test]
fn rotate_revoke_and_other_actor_cannot_reuse_credential() {
    let (_temp,home,created) = fixture();
    let rotated = configure(&home,"g_test","hark","generation",true).expect("rotate");
    assert_eq!(created["connector"]["connector_id"],rotated["connector"]["connector_id"]);
    assert!(!secret_matches(&rotated["connector"],created["secret"].as_str().expect("old")));
    let other = configure(&home,"g_other","hark","generation",false).expect("other");
    assert_ne!(rotated["connector"]["connector_id"],other["connector"]["connector_id"]);
    assert!(!secret_matches(&other["connector"],rotated["secret"].as_str().expect("secret")));
    revoke(&home,rotated["connector"]["connector_id"].as_str().expect("id")).expect("revoke");
    let revoked = load(&home).expect("load").into_iter().find(|c| c["connector_id"] == rotated["connector"]["connector_id"]).expect("record");
    assert!(resolve(&revoked,"","cccc_runtime_wait_next_turn").is_none());
}

#[test]
fn execution_is_turn_bound_rotatable_and_completion_only_after_finish() {
    let (_temp,home,created) = fixture();
    let mut c = created["connector"].clone();
    let b = resolve(&c,"","cccc_runtime_wait_next_turn").expect("claim");
    c["hark"]["execution"] = json!({"turn_id":"turn_one","state":"active"});
    let credential = token(&c,&b,&c["hark"],"turn_one").expect("token");
    assert_eq!(resolve(&c,&credential,"cccc_message_send").expect("execution")["hark_turn_id"],"turn_one");
    assert!(resolve(&c,&credential,"cccc_runtime_wait_next_turn").is_none());
    let other = configure(&home,"g_test","other","generation",false).expect("other");
    assert!(resolve(&other["connector"],&credential,"cccc_message_send").is_none());
    c["hark"]["last_execution"] = json!({"turn_id":"turn_one","state":"completed"});
    c["hark"]["execution"] = Value::Null;
    assert!(resolve(&c,&credential,"cccc_message_send").is_none());
    assert_eq!(resolve(&c,&credential,"cccc_runtime_complete_turn").expect("duplicate completion")["hark_stage"],"completion");
    c["hark"]["epoch"] = json!("rotated");
    assert!(resolve(&c,&credential,"cccc_runtime_complete_turn").is_none());
}
