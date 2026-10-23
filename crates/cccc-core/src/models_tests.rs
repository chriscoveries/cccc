use crate::models::{ModelRegistry, ModelSpec, TurnCost};
use serde_json::json;

#[test]
fn resolves_real_catalog_values() {
    let mut reg = ModelRegistry::new();
    reg.models.insert(
        "priced-model".to_string(),
        ModelSpec {
            usd_per_mtok: Some(0.07),
            strength: Some(3),
            code: true,
            ..Default::default()
        },
    );
    reg.models.insert(
        "free".to_string(),
        ModelSpec {
            usd_per_mtok: Some(0.0),
            strength: Some(2),
            code: false,
            ..Default::default()
        },
    );
    let (cost, strength) = reg.resolve("priced-model");
    assert_eq!(cost, Some(0.07));
    assert_eq!(strength, Some(3));
    let (cost, strength) = reg.resolve("free");
    assert_eq!(cost, Some(0.0));
    assert_eq!(strength, Some(2));
    let (cost, strength) = reg.resolve("unknown");
    assert_eq!(cost, None);
    assert_eq!(strength, None);
    let (cost, strength) = reg.resolve("");
    assert_eq!(cost, None);
    assert_eq!(strength, None);
}

/// A captured Claude turn's counters: 2 input, 40 output, 213,575 cache reads
/// and 905 cache writes.
fn captured_usage() -> serde_json::Value {
    json!({"input_tokens": 2, "output_tokens": 40,
           "cache_read_tokens": 213575, "cache_write_tokens": 905})
}

#[test]
fn a_turn_costs_its_metered_tokens_at_the_catalog_price() {
    let mut reg = ModelRegistry::new();
    reg.models.insert(
        "priced".into(),
        ModelSpec {
            usd_per_mtok: Some(0.07),
            ..Default::default()
        },
    );
    // 214_522 tokens x 0.07 per million.
    match reg.turn_cost(Some("priced"), &captured_usage()) {
        TurnCost::Priced { usd, tokens } => {
            assert_eq!(tokens, 214_522);
            assert!((usd - 0.01501654).abs() < 1e-12, "{usd}");
        }
        other => panic!("expected a priced turn, got {other:?}"),
    }
    // A free model is priced at zero, which is a price, not an absence of one.
    reg.models.insert(
        "free".into(),
        ModelSpec {
            usd_per_mtok: Some(0.0),
            ..Default::default()
        },
    );
    assert_eq!(
        reg.turn_cost(Some("free"), &captured_usage()),
        TurnCost::Priced {
            usd: 0.0,
            tokens: 214_522
        }
    );
}

#[test]
fn an_unpriced_model_and_an_unmetered_turn_are_told_apart() {
    let mut reg = ModelRegistry::new();
    reg.models.insert(
        "flat".into(),
        ModelSpec {
            usd_per_month: Some(400.0),
            ..Default::default()
        },
    );
    // Tokens, but no per-token price to apply to them.
    assert_eq!(
        reg.turn_cost(Some("flat"), &captured_usage()),
        TurnCost::Unpriced { tokens: 214_522 }
    );
    assert_eq!(
        reg.turn_cost(Some("not-in-the-catalog"), &captured_usage()),
        TurnCost::Unpriced { tokens: 214_522 }
    );
    assert_eq!(
        reg.turn_cost(None, &captured_usage()),
        TurnCost::Unpriced { tokens: 214_522 }
    );
    // No counters at all: nothing to price, and not a free turn either.
    assert_eq!(
        reg.turn_cost(Some("flat"), &json!({"model_id": "flat"})),
        TurnCost::Unmetered
    );
    // A zero counter is a counter: the turn really did report nothing spent.
    assert_eq!(
        reg.turn_cost(
            Some("flat"),
            &json!({"input_tokens": 0, "output_tokens": 0})
        ),
        TurnCost::Unpriced { tokens: 0 }
    );
}

#[test]
fn registry_default_empty() {
    let reg = ModelRegistry::new();
    assert!(reg.models.is_empty());
    assert_eq!(reg.get("priced-model"), None);
}
