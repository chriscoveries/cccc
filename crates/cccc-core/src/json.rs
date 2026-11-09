//! Shared serde_json `Value` helpers used across the workspace.

use serde_json::{Map, Value, json};

/// Ensures `value` holds a JSON object (initializing it to `{}` when it does
/// not) and returns the object as a mutable map. `label` is the panic text
/// should the value somehow still not be an object — callers keep their own
/// site name there.
pub fn ensure_object<'a>(value: &'a mut Value, label: &str) -> &'a mut Map<String, Value> {
    if !value.is_object() {
        *value = json!({});
    }
    value.as_object_mut().expect(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_object_initializes_a_non_object() {
        let mut value = json!(null);
        let map = ensure_object(&mut value, "test object initialized");
        map.insert("k".into(), json!(1));
        assert_eq!(value, json!({"k": 1}));
    }

    #[test]
    fn ensure_object_keeps_an_existing_object() {
        let mut value = json!({"a": 1});
        let map = ensure_object(&mut value, "test object initialized");
        map.insert("b".into(), json!(2));
        assert_eq!(value, json!({"a": 1, "b": 2}));
    }
}
