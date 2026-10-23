use crate::models::{ModelRegistry, ModelSpec};

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

#[test]
fn registry_default_empty() {
    let reg = ModelRegistry::new();
    assert!(reg.models.is_empty());
    assert_eq!(reg.get("priced-model"), None);
}
