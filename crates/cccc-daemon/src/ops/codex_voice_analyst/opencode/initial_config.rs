//! Apply an explicit initial model to process-local configuration only.
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::io;

const KEY: &str = "OPENCODE_CONFIG_CONTENT";

fn invalid_config() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "Cannot apply explicit OpenCode model: OPENCODE_CONFIG_CONTENT must be a valid JSON/JSONC object",
    )
}

pub(super) fn apply(
    environment: &mut BTreeMap<String, String>,
    model: Option<&str>,
) -> io::Result<()> {
    let Some(model) = model.map(str::trim).filter(|value| !value.is_empty()) else {
        // Leave opaque native configuration, including inherited bytes, alone.
        return Ok(());
    };
    let content = match environment.get(KEY) {
        Some(value) => Some(value.clone()),
        None => std::env::var_os(KEY)
            .map(|value| value.into_string().map_err(|_| invalid_config()))
            .transpose()?,
    };
    let mut config = content
        .as_deref()
        .map(parse_object)
        .transpose()?
        .unwrap_or_default();
    config.insert("model".into(), Value::String(model.into()));
    let merged = serde_json::to_string(&config).map_err(|_| invalid_config())?;
    environment.insert(KEY.into(), merged);
    Ok(())
}

/// Remove JSONC comments and trailing commas, then let serde validate all
/// remaining JSON grammar. Strings and Unicode bytes are preserved. Replacing
/// comments with whitespace prevents them from joining otherwise invalid tokens.
fn parse_object(content: &str) -> io::Result<Map<String, Value>> {
    let mut bytes = content.as_bytes().to_vec();
    let mut index = 0;
    let mut quoted = false;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else if byte == b'"' {
            quoted = true;
        } else if bytes[index..].starts_with(b"//") {
            while index < bytes.len() && !matches!(bytes[index], b'\r' | b'\n') {
                bytes[index] = b' ';
                index += 1;
            }
            continue;
        } else if bytes[index..].starts_with(b"/*") {
            let end = bytes[index + 2..]
                .windows(2)
                .position(|pair| pair == b"*/")
                .map(|offset| index + 2 + offset + 2)
                .ok_or_else(invalid_config)?;
            for byte in &mut bytes[index..end] {
                if !matches!(*byte, b'\r' | b'\n') {
                    *byte = b' ';
                }
            }
            index = end;
            continue;
        }
        index += 1;
    }

    quoted = false;
    escaped = false;
    let mut previous = None;
    for index in 0..bytes.len() {
        let byte = bytes[index];
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
                previous = Some(b'"');
            }
            continue;
        }
        match byte {
            b'"' => quoted = true,
            b',' => {
                let next = bytes[index + 1..]
                    .iter()
                    .find(|byte| !json_whitespace(**byte))
                    .copied();
                // A comma without a preceding value is not a trailing comma:
                // never turn [,], {,}, or a missing property value into valid JSON.
                if !matches!(previous, None | Some(b'[' | b'{' | b',' | b':'))
                    && matches!(next, Some(b']' | b'}'))
                {
                    bytes[index] = b' ';
                } else {
                    previous = Some(byte);
                }
            }
            byte if json_whitespace(byte) => {}
            byte => previous = Some(byte),
        }
    }
    match serde_json::from_slice(&bytes).map_err(|_| invalid_config())? {
        Value::Object(object) => Ok(object),
        _ => Err(invalid_config()),
    }
}

fn json_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn comments_and_nested_trailing_commas_preserve_values() {
        let parsed =
            parse_object("{/*é*/\"items\":[1,true,null,],//line\r\n\"nested\":{\"x\":2,},}")
                .expect("JSONC object");
        assert_eq!(
            Value::Object(parsed),
            json!({"items":[1,true,null],"nested":{"x":2}})
        );
    }

    #[test]
    fn comment_markers_escapes_and_unicode_inside_strings_are_untouched() {
        let original = json!({"url":"https://example.test/*data*/", "text":"é \\\" // */ ,]"});
        let parsed = parse_object(&original.to_string()).expect("JSON strings");
        assert_eq!(Value::Object(parsed), original);
    }

    #[test]
    fn missing_values_and_javascript_extensions_stay_invalid() {
        for input in [
            "{,}",
            "{\"a\":[,]}",
            "{\"a\":,}",
            "{\"a\":1,,}",
            "{a:1}",
            "{'a':1}",
            "{\"a\":NaN}",
            "{\"a\":tru/*x*/e}",
            "{/*unclosed",
        ] {
            assert_eq!(
                parse_object(input).expect_err(input).kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn non_objects_and_invalid_content_have_a_static_diagnostic() {
        for input in ["[]", "null", "false", "\"secret-value\"", "{secret-value"] {
            let error = parse_object(input).expect_err("object required");
            assert_eq!(error.to_string(), invalid_config().to_string());
            assert!(!error.to_string().contains("secret-value"));
        }
    }

    #[test]
    fn explicit_model_overrides_only_the_model_field() {
        let mut environment = BTreeMap::from([(
            KEY.into(),
            r#"{"model":"old/one","provider":{"keep":[1,2]},"note":"/*data*/"}"#.into(),
        )]);
        apply(&mut environment, Some("new/two")).expect("merge model");
        let value: Value = serde_json::from_str(&environment[KEY]).expect("canonical JSON");
        assert_eq!(
            value,
            json!({"model":"new/two","provider":{"keep":[1,2]},"note":"/*data*/"})
        );
    }

    #[test]
    fn without_model_even_invalid_actor_config_is_byte_identical() {
        let mut environment = BTreeMap::from([(KEY.into(), "{opaque-invalid".into())]);
        let before = environment.clone();
        apply(&mut environment, None).expect("native validation owns this");
        assert_eq!(environment, before);
    }
}
