#[path = "initial_native_turn/evidence.rs"]
mod evidence;

use evidence::{Capture, Fixture};

fn fixture() -> Fixture {
    let fixture: Fixture = serde_json::from_str(include_str!("initial_native_turn/captures.json"))
        .expect("captured initial-turn evidence must parse");
    assert_eq!(fixture.schema, 1);
    assert_eq!(fixture.provider, "historical_scripted_loopback");
    assert_eq!(
        fixture.source_sha,
        "479b3160a857644af912625829d0b00592146551"
    );
    for digest in [&fixture.binary_sha256, &fixture.provider_fixture_sha256] {
        assert_eq!(digest.len(), 64);
    }
    fixture
}

fn arm<'a>(f: &'a Fixture, name: &str) -> &'a Capture {
    f.cases.iter().find(|c| c.name == name).expect(name)
}

fn early(runtime: &str) {
    let f = fixture();
    let c = arm(&f, &format!("{runtime}-mcp"));
    assert_eq!(c.runtime, runtime);
    assert!(!c.runtime_version.is_empty());
    assert_eq!(c.runtime_sha256.len(), 64);
    assert!(c.native.users.is_empty() && c.native.assistants.is_empty());
    assert_eq!(c.authenticated_requests, 0);
    let r = c.verify(&c.native);
    assert_eq!(r.transport, "accepted");
    assert_eq!(r.delivery_event_ids.len(), 2);
    assert!(!r.observed_admission);
    assert!(!r.execution);
    assert_eq!(r.backend_outcome, "unobserved");
    assert_eq!(
        r.observed_liveness, "FAIL",
        "accepted transport is not initial-turn execution"
    );
    assert_eq!(r.full_proof, "INCONCLUSIVE");
    eprintln!("{r:?}");
}

#[test]
fn opencode_accepted_transport_without_native_input_fails_liveness() {
    early("opencode");
}

#[test]
fn kilo_accepted_transport_without_native_input_fails_liveness() {
    early("kilo");
}

fn rendered(runtime: &str) {
    let f = fixture();
    let c = arm(&f, &format!("{runtime}-rendered"));
    assert_eq!(c.native.users.len(), 1);
    assert_eq!(c.native.assistants.len(), 3);
    assert_eq!(c.authenticated_requests, 3);
    let r = c.verify(&c.native);
    assert_eq!(r.transport, "accepted");
    assert!(r.observed_admission && r.execution);
    assert_eq!(r.native_user_id, Some(c.native.users[0].id.as_str()));
    assert_eq!(
        r.native_part_id,
        Some(c.native.users[0].parts[0].id.as_str())
    );
    assert_eq!(r.backend_outcome, "completed");
    assert_eq!(r.observed_liveness, "fixture-completed");
    assert_eq!(r.prepared_payload_match, "unknown");
    assert_eq!(r.production_admission, "unknown");
    assert_eq!(
        r.full_proof, "INCONCLUSIVE",
        "a scripted capture cannot certify real inference"
    );
    eprintln!("{r:?}");
}

#[test]
fn opencode_rendered_control_has_correlated_fixture_execution() {
    rendered("opencode");
}

#[test]
fn kilo_rendered_control_has_correlated_fixture_execution() {
    rendered("kilo");
}

#[test]
fn a_different_real_source_or_session_cannot_supply_execution() {
    let f = fixture();
    // Select other genuine arms; do not manufacture ids, roles or timestamps.
    for c in &f.cases {
        for other in &f.cases {
            if c.name == other.name {
                continue;
            }
            let r = c.verify(&other.native);
            assert!(
                !r.observed_admission && !r.execution,
                "{} vs {}",
                c.name,
                other.name
            );
            assert_ne!(r.backend_outcome, "completed");
            assert_ne!(r.observed_liveness, "fixture-completed");
            assert_eq!(r.full_proof, "INCONCLUSIVE");
        }
    }
}
