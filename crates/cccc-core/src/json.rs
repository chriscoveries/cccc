use serde_json::{Map, Value, json};

/// Return a mutable object, replacing any non-object with an empty one.
pub fn object_mut(value: &mut Value) -> &mut Map<String, Value> {
    if !value.is_object() {
        *value = json!({});
    }
    value.as_object_mut().expect("object initialized")
}

/// Trim leading and trailing whitespace.
pub fn trimmed(value: &str) -> String {
    value.trim().to_owned()
}

/// Trim a string and keep at most `max_chars`, ending a longer value in `...`.
pub fn trimmed_to(value: &str, max_chars: usize) -> String {
    let text = trimmed(value);
    if text.chars().count() <= max_chars {
        return text;
    }
    if max_chars <= 3 {
        return text.chars().take(max_chars).collect();
    }
    format!(
        "{}...",
        text.chars()
            .take(max_chars - 3)
            .collect::<String>()
            .trim_end()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_mut_replaces_a_non_object() {
        let mut value = json!([]);

        assert!(object_mut(&mut value).is_empty());
        assert_eq!(value, json!({}));
    }

    #[test]
    fn trimmed_removes_surrounding_whitespace() {
        assert_eq!(trimmed("  text\n"), "text");
        assert_eq!(trimmed(" \t "), "");
    }

    #[test]
    fn trimmed_to_keeps_short_text_and_marks_a_cut() {
        assert_eq!(trimmed_to("  short  ", 8), "short");
        assert_eq!(trimmed_to("abcdefgh", 8), "abcdefgh");
        assert_eq!(trimmed_to("abcdefghi", 8), "abcde...");
        assert_eq!(trimmed_to("abcdef", 3), "abc");
    }
}
