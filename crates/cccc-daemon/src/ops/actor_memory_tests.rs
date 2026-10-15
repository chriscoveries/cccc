use cccc_contracts::Actor;
use serde_json::json;

#[test]
fn actor_memory_limit_round_trips_and_defaults_off() {
    let original = Actor::new("memory-worker");
    let default = serde_json::to_value(&original).expect("serialize default actor");
    assert!(default.get("memory_limit_mb").is_none());
    let mut configured = default;
    configured["memory_limit_mb"] = json!(256);
    let actor: Actor = serde_json::from_value(configured).expect("configured actor");
    assert_eq!(
        serde_json::to_value(actor).expect("serialize configured")["memory_limit_mb"],
        json!(256)
    );
}

#[test]
fn actor_memory_limit_rejects_negative_fractional_and_overflow_values() {
    for invalid in [
        json!(-1),
        json!(1.5),
        json!("256"),
        json!(18446744073709551615_u64),
    ] {
        let mut configured =
            serde_json::to_value(Actor::new("memory-worker")).expect("serialize default actor");
        configured["memory_limit_mb"] = invalid;
        assert!(serde_json::from_value::<Actor>(configured).is_err());
    }
}
