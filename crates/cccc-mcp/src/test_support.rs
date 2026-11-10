use serde_json::{Map, Value};

pub(crate) fn object(value: Value) -> Map<String, Value> {
    value.as_object().cloned().expect("object")
}
