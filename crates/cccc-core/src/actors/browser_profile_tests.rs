use super::*;
use serde_json::json;

// Redacted live metadata retained in the adjacent fixture provenance. These
// are labeled configuration mutations, not evidence of two provider accounts.
fn captured_actor(group: &GroupDoc) -> &Actor {
    group
        .actors
        .iter()
        .find(|actor| actor.id == "web1")
        .expect("captured browser actor")
}

fn captured() -> GroupDoc {
    serde_json::from_str(include_str!(
        "../../tests/fixtures/browser-profile-group.json"
    ))
    .expect("retained metadata capture")
}

#[test]
fn captured_default_keeps_wire_and_route_identity() {
    let group = captured();
    let actor = captured_actor(&group);
    let wire = serde_json::to_value(actor).expect("encode");
    assert!(wire.get("browser_profile").is_none());
    assert!(wire.get("browser_profile_revision").is_none());
    assert_eq!(browser_profile(actor), "_shared");
    assert_eq!(
        web_model_generation_identity(actor),
        generation_identity(actor)
    );
    let mut control = group.clone();
    let same = update(
        &mut control,
        "web1",
        json!({"browser_profile":"_shared"})
            .as_object()
            .expect("patch"),
    )
    .expect("same selection");
    assert_eq!(same.browser_profile_revision, 0);
    assert_eq!(
        web_model_generation_identity(&same),
        generation_identity(actor)
    );
}

#[test]
fn profile_aba_changes_only_browser_authority_and_survives_roundtrip() {
    let mut group = captured();
    let registration = generation_identity(captured_actor(&group));
    let mut identities = std::collections::BTreeSet::new();
    for (index, name) in ["secondary", "_shared", "secondary"].iter().enumerate() {
        let actor = update(
            &mut group,
            "web1",
            json!({"browser_profile":name}).as_object().expect("patch"),
        )
        .expect("selection mutation");
        assert_eq!(actor.browser_profile_revision, index as u64 + 1);
        assert_eq!(
            generation_identity(&actor),
            registration,
            "native source identity must not change"
        );
        assert!(
            identities.insert(web_model_generation_identity(&actor)),
            "old profile authority cannot revive"
        );
        group =
            serde_json::from_value(serde_json::to_value(&group).expect("encode")).expect("reload");
    }
    let before = captured_actor(&group).clone();
    let same = update(
        &mut group,
        "web1",
        json!({"browser_profile":"secondary"})
            .as_object()
            .expect("patch"),
    )
    .expect("idempotent edit");
    assert_eq!(
        web_model_generation_identity(&before),
        web_model_generation_identity(&same)
    );
}

#[test]
fn caller_revision_and_invalid_names_or_types_leave_capture_unchanged() {
    for patch in [
        json!({"browser_profile_revision":9}),
        json!({"browser_profile":"../secondary"}),
        json!({"browser_profile":"Secondary"}),
        json!({"browser_profile":"."}),
        json!({"browser_profile":"_private"}),
        json!({"browser_profile":"secondary/account"}),
        json!({"browser_profile":"x".repeat(49)}),
        json!({"browser_profile":false}),
        json!({"browser_profile":null}),
    ] {
        let mut group = captured();
        let before = group.actors.clone();
        assert!(update(&mut group, "web1", patch.as_object().expect("mutation")).is_err());
        assert_eq!(group.actors, before);
    }
}

#[test]
fn exhausted_selection_revision_cannot_wrap_or_replace_the_capture() {
    let mut group = captured();
    group
        .actors
        .iter_mut()
        .find(|a| a.id == "web1")
        .expect("actor")
        .browser_profile_revision = u64::MAX;
    let before = group.actors.clone();
    assert!(
        update(
            &mut group,
            "web1",
            json!({"browser_profile":"secondary"})
                .as_object()
                .expect("patch")
        )
        .is_err()
    );
    assert_eq!(group.actors, before);
}
