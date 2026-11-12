use crate::{GroupStore, HomeLayout, actors};
use cccc_contracts::{Actor, web_model_pin::WebModelPin};
use serde_json::{Value, json};

fn captured_actor() -> Actor {
    serde_json::from_str(include_str!("../tests/fixtures/web-model-pin/actor.json"))
        .expect("redacted real Actor metadata")
}

fn group() -> (tempfile::TempDir, crate::GroupDoc) {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    let mut group = GroupStore::new(home)
        .expect("store")
        .create("", "")
        .expect("test storage envelope");
    actors::add(&mut group, captured_actor()).expect("captured Actor add");
    (temp, group)
}

fn update(group: &mut crate::GroupDoc, patch: Value) -> std::io::Result<Actor> {
    actors::update(group, "browser", patch.as_object().expect("patch"))
}

#[test]
fn absent_pin_preserves_captured_metadata_and_descriptive_label() {
    let (_temp, mut group) = group();
    let actor = update(&mut group, json!({"model_label":"operator declaration"}))
        .expect("descriptive label control");
    assert!(actor.web_model_pin.is_none());
    assert_eq!(actor.web_model_pin_revision, 0);
    let serialized = serde_json::to_value(actor).expect("round trip");
    assert!(serialized.get("web_model_pin").is_none());
    assert!(serialized.get("web_model_pin_revision").is_none());
}

#[test]
fn policy_revision_fences_aba_and_ignores_equivalent_deny_order() {
    let (_temp, mut group) = group();
    // Policy controls on the captured Actor, not a claimed browser capture.
    let first = json!({"model":"gpt-6","effort":"extra_high","deny":[
        {"model":"gpt-6","effort":"instant"},
        {"model":"gpt-6","effort":"medium"}
    ]});
    let a = update(&mut group, json!({"web_model_pin":first})).expect("pin A");
    assert_eq!(a.web_model_pin_revision, 1);
    let reordered = json!({"model":"gpt-6","effort":"extra_high","deny":[
        {"model":"gpt-6","effort":"medium"},
        {"model":"gpt-6","effort":"instant"},
        {"model":"gpt-6","effort":"instant"}
    ]});
    let equivalent = update(&mut group, json!({"web_model_pin":reordered}))
        .expect("equivalent normalized policy");
    assert_eq!(equivalent.web_model_pin_revision, 1);
    let b = update(&mut group, json!({"web_model_pin":{"model":"gpt-5.6-pro"}})).expect("pin B");
    assert_eq!(b.web_model_pin_revision, 2);
    let a_again = update(&mut group, json!({"web_model_pin":first})).expect("pin A again");
    assert_eq!(a_again.web_model_pin_revision, 3);
    assert_ne!(a.web_model_pin_revision, a_again.web_model_pin_revision);
    let cleared = update(&mut group, json!({"web_model_pin":null})).expect("clear pin");
    assert_eq!(cleared.web_model_pin_revision, 4);
}

#[test]
fn rejected_policy_mutations_leave_actor_and_revision_unchanged() {
    let (_temp, mut group) = group();
    let accepted = update(
        &mut group,
        json!({"web_model_pin":{"model":"gpt-6","effort":"extra_high"}}),
    )
    .expect("allowed configuration control");
    let before = serde_json::to_value(&group.actors).expect("before");
    // Fault mutations only: no model is selected and no browser is opened.
    for policy in [
        json!(false),
        json!([]),
        json!({}),
        json!({"model":"unknown"}),
        json!({"model":"gpt-6-pro"}),
        json!({"model":"gpt-6","effort":"pro"}),
        json!({"model":"gpt-6","effort":4}),
        json!({"model":"gpt-5.6-pro","effort":"high"}),
        json!({"model":"gpt-6","on_unavailable":"automatic"}),
        json!({"model":"gpt-6","on_unavailable":"fallback"}),
        json!({"model":"gpt-6","fallback":{"model":"gpt-5.6-pro"}}),
        json!({"model":"gpt-6","on_unavailable":"fallback","fallback":{"model":"gpt-6-pro"}}),
        json!({"model":"gpt-6","deny":[{"model":"gpt-6"}]}),
        json!({"model":"gpt-6","unknown":true}),
    ] {
        assert!(update(&mut group, json!({"web_model_pin":policy})).is_err());
        assert_eq!(serde_json::to_value(&group.actors).expect("after"), before);
    }
    for patch in [
        json!({"web_model_pin_revision":0}),
        json!({"web_model_pin_revision":accepted.web_model_pin_revision}),
        json!({"runtime":"grok_web_model"}),
    ] {
        assert!(update(&mut group, patch).is_err());
        assert_eq!(serde_json::to_value(&group.actors).expect("after"), before);
    }
}

#[test]
fn creation_cannot_supply_a_revision_or_pin_another_runtime() {
    let (_temp, mut group) = group();
    group.actors.clear();
    let mut actor = captured_actor();
    actor.web_model_pin_revision = 1; // Forged revision mutation.
    assert!(actors::add(&mut group, actor).is_err());
    assert!(group.actors.is_empty());
    let mut actor = captured_actor();
    actor.runtime = cccc_contracts::ActorRuntime::GrokWebModel;
    actor.web_model_pin =
        Some(serde_json::from_value(json!({"model":"gpt-6"})).expect("pin control"));
    assert!(actors::add(&mut group, actor).is_err());
    assert!(group.actors.is_empty());
}

#[test]
fn explicit_fallback_and_deny_are_validated_without_silent_substitution() {
    let (_temp, mut group) = group();
    let policy = json!({
        "model":"gpt-6", "effort":"extra_high", "on_unavailable":"fallback",
        "fallback":{"model":"gpt-5.6-pro"},
        "deny":[{"model":"gpt-6","effort":"instant"}]
    });
    let actor = update(&mut group, json!({"web_model_pin":policy})).expect("explicit fallback");
    let stored = serde_json::to_value(actor.web_model_pin).expect("stored policy");
    assert_eq!(stored["model"], "gpt-6");
    assert_eq!(stored["fallback"]["model"], "gpt-5.6-pro");
    let before = serde_json::to_value(&group.actors).expect("before");
    let mut denied = policy.clone();
    denied["deny"] = json!([{"model":"gpt-5.6-pro"}]);
    assert!(update(&mut group, json!({"web_model_pin":denied})).is_err());
    let mut over_limit = policy;
    over_limit["deny"] = json!(vec![json!({"model":"gpt-6","effort":"instant"}); 17]);
    assert!(update(&mut group, json!({"web_model_pin":over_limit})).is_err());
    assert_eq!(serde_json::to_value(&group.actors).expect("after"), before);
}

#[test]
fn revision_overflow_cannot_commit_a_changed_policy() {
    let (_temp, mut group) = group();
    // Explicit stored-state boundary mutation, not a captured revision value.
    group.actors[0].web_model_pin_revision = u64::MAX;
    let before = serde_json::to_value(&group.actors).expect("before");
    assert!(update(&mut group, json!({"web_model_pin":{"model":"gpt-6"}})).is_err());
    assert_eq!(serde_json::to_value(&group.actors).expect("after"), before);
}

#[test]
fn unconstrained_effort_does_not_authorize_an_unknown_observation() {
    let pin: WebModelPin =
        serde_json::from_value(json!({"model":"gpt-6"})).expect("policy control");
    assert!(!pin.primary().accepts_observed(pin.primary()));
    let observed = serde_json::from_value(json!({"model":"gpt-6","effort":"extra_high"}))
        .expect("canonical observation control, not a DOM capture");
    assert!(pin.primary().accepts_observed(observed));
}

#[test]
fn observed_policy_checks_keep_deny_and_fallback_authority_separate() {
    let mut pin: WebModelPin = serde_json::from_value(json!({
        "model":"gpt-6", "on_unavailable":"fallback",
        "fallback":{"model":"gpt-5.6-pro"},
        "deny":[{"model":"gpt-6","effort":"instant"}]
    }))
    .expect("policy control on captured runtime");
    pin.normalize().expect("valid policy");
    let denied = serde_json::from_value(json!({"model":"gpt-6","effort":"instant"}))
        .expect("denied canonical observation control");
    assert!(!pin.accepts_primary_observation(denied));
    assert!(!pin.accepts_fallback_observation(denied));
    let fallback = pin.fallback.expect("explicit fallback");
    assert!(!pin.accepts_primary_observation(fallback));
    assert!(pin.accepts_fallback_observation(fallback));
    pin.on_unavailable = cccc_contracts::web_model_pin::ModelPinUnavailable::Hold;
    assert!(!pin.accepts_fallback_observation(fallback));
}
