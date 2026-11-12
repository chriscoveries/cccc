pub(super) use super::inbound_attachments::MAX_ATTACHMENT_BYTES;
use cccc_contracts::Event;
use cccc_core::HomeLayout;
use serde_json::Value;
use std::path::{Path, PathBuf};

pub(super) fn attachments(event: &Event) -> &[Value] {
    event
        .data
        .get("attachments")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

pub(super) async fn load_blob(
    home: &HomeLayout,
    group_id: &str,
    value: &Value,
) -> Result<(PathBuf, Vec<u8>), String> {
    let relative = value
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| "attachment path is missing".to_owned())?;
    let path =
        cccc_core::blobs::resolve(home, group_id, relative).map_err(|error| error.to_string())?;
    if path.metadata().map_err(|error| error.to_string())?.len() > MAX_ATTACHMENT_BYTES {
        return Err("attachment exceeds 10 MiB before read".into());
    }
    let raw = tokio::fs::read(&path)
        .await
        .map_err(|error| error.to_string())?;
    validate_loaded_size(&raw)?;
    Ok((path, raw))
}

pub(super) fn validate_loaded_size(raw: &[u8]) -> Result<(), String> {
    (raw.len() as u64 <= MAX_ATTACHMENT_BYTES)
        .then_some(())
        .ok_or_else(|| "attachment exceeds 10 MiB after read".into())
}

pub(super) fn filename<'a>(value: &'a Value, path: &'a Path) -> &'a str {
    value
        .get("title")
        .and_then(Value::as_str)
        .and_then(safe_filename)
        .or_else(|| path.file_name().and_then(|name| name.to_str()))
        .unwrap_or("file")
}

pub(super) fn mime_type<'a>(value: &'a Value, fallback: &Path) -> &'a str {
    value
        .get("mime_type")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .or_else(|| mime_guess::from_path(fallback).first_raw())
        .unwrap_or("application/octet-stream")
}

/// Accept only a single, portable file-name component for outbound uploads.
pub(super) fn safe_filename(value: &str) -> Option<&str> {
    let value = value.trim();
    let path = Path::new(value);
    (!value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains(['/', '\\'])
        && !value.chars().any(char::is_control)
        && path.file_name().and_then(|name| name.to_str()) == Some(value))
    .then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_portable_single_components() {
        assert_eq!(safe_filename(" report.md "), Some("report.md"));
        for value in [
            "",
            ".",
            "..",
            "../report.md",
            "folder/report.md",
            "folder\\report.md",
            "bad\nname",
        ] {
            assert_eq!(safe_filename(value), None, "{value:?}");
        }
    }
}
