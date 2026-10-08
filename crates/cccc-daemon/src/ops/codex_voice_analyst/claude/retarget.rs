//! Read-only preflight for an operator-selected conversation.
use super::*;
use crate::dispatch::OpError;
use std::io::{BufRead, BufReader, Read};

pub(crate) fn validate_retarget(
    environment: &BTreeMap<String, String>,
    cwd: &Path,
    session_id: &str,
) -> Result<Option<String>, OpError> {
    let config = command::existing_config_dir(environment).map_err(|error| {
        OpError::new("claude_config_unavailable", format!("The actor's Claude config directory cannot be resolved without creating or changing it: {error}"))
    })?;
    let workspace = cwd.canonicalize().map_err(OpError::io)?;
    let projects = config.join("projects");
    let transcript = projects
        .join(project_slug(&workspace))
        .join(format!("{session_id}.jsonl"));
    let metadata = std::fs::symlink_metadata(&transcript).map_err(|error| OpError::new("claude_transcript_missing", format!("Claude transcript for session {session_id} is unavailable in this actor's CLAUDE_CONFIG_DIR/workspace: {} ({error})", transcript.display())))?;
    let canonical = transcript.canonicalize().map_err(OpError::io)?;
    if !metadata.file_type().is_file()
        || !canonical.starts_with(projects.canonicalize().map_err(OpError::io)?)
        || canonical != transcript
    {
        return Err(OpError::new(
            "claude_transcript_invalid",
            "Claude transcript must be a regular file inside this account's project directory, without symlinks",
        ));
    }
    let mut recorded_cwd = false;
    let mut retained_model = None;
    let mut reader = BufReader::new(std::fs::File::open(&transcript).map_err(OpError::io)?);
    let mut total = 0usize;
    loop {
        let mut line = Vec::new();
        let count = reader
            .by_ref()
            .take((MAX_TRANSCRIPT_LINE_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)
            .map_err(OpError::io)?;
        if count == 0 {
            break;
        }
        total = total.saturating_add(count);
        if count > MAX_TRANSCRIPT_LINE_BYTES || total > 256 * 1024 * 1024 {
            return Err(OpError::new(
                "claude_transcript_invalid",
                "Claude transcript exceeds the validation size limit",
            ));
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let value: Value = serde_json::from_slice(&line).map_err(|error| {
            OpError::new(
                "claude_transcript_invalid",
                format!("Claude transcript contains invalid JSON: {error}"),
            )
        })?;
        if let Some(id) = value.get("sessionId").and_then(Value::as_str)
            && id != session_id
        {
            return Err(OpError::new(
                "claude_transcript_invalid",
                "Claude transcript records a different session ID",
            ));
        }
        if value.get("type").and_then(Value::as_str) == Some("assistant")
            && let Some(model) = value.pointer("/message/model").and_then(Value::as_str)
            && !model.is_empty()
            && !model.starts_with('<')
        {
            retained_model = Some(model.to_owned());
        }
        if let Some(path) = value.get("cwd").and_then(Value::as_str) {
            recorded_cwd = true;
            if Path::new(path)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(path))
                != workspace
            {
                return Err(OpError::new(
                    "claude_workspace_mismatch",
                    format!(
                        "Session {session_id} records cwd '{path}', but this actor launches in '{}'",
                        workspace.display()
                    ),
                ));
            }
        }
    }
    if !recorded_cwd {
        return Err(OpError::new(
            "claude_workspace_mismatch",
            "Claude transcript has no recorded cwd; its workspace cannot be verified",
        ));
    }
    let live_config = config.clone();
    let id = session_id.to_owned();
    // Use the same daemon-owned async runtime as managed launch, including sync IPC callers.
    super::super::super::local_headless::run_managed_launch(async move {
        ensure_available(&live_config, &id).await
    })
    .map_err(|error| {
        if error.kind() == io::ErrorKind::WouldBlock {
            OpError::new("claude_session_live", error.to_string())
        } else {
            OpError::new(
                "claude_session_check_failed",
                format!("Could not verify whether Claude session {session_id} is live: {error}"),
            )
        }
    })?;
    Ok(retained_model)
}

/// Recheck at launch; explicit retarget never adopts another process's live job.
pub(super) async fn ensure_available(config: &Path, session_id: &str) -> io::Result<()> {
    if let Some((_, job)) = find_live_job(config, session_id).await? {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!(
                "Session {session_id} is live in Claude Agent View job {}. Stop it through its owner before resuming; CCCC will not stop another session",
                job.short
            ),
        ));
    }
    if let Some(pid) = registry_holder(config, session_id)? {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!(
                "Session {session_id} is held by another Claude process (pid {pid}). Close it through its owner before resuming; CCCC will not kill a foreign process"
            ),
        ));
    }
    Ok(())
}

fn registry_holder(config: &Path, session_id: &str) -> io::Result<Option<u32>> {
    let entries = match std::fs::read_dir(config.join("sessions")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let Some(pid) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if path.extension().and_then(|s| s.to_str()) != Some("json")
            || !entry.file_type()?.is_file()
        {
            continue;
        }
        if !super::super::super::membership_cloudflared::process_is_alive(pid) {
            continue;
        }
        let value: Value = match cccc_core::fs::read_json(&path) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if value
            .get("sessionId")
            .and_then(Value::as_str)
            .is_some_and(|id| id.eq_ignore_ascii_case(session_id))
        {
            return Ok(Some(pid));
        }
    }
    Ok(None)
}

/// Claude replaces every non-ASCII alphanumeric UTF-16 unit with '-'. Long
/// project keys use the first 200 units and a base36 JavaScript string hash.
fn project_slug(cwd: &Path) -> String {
    let path = cwd.to_string_lossy();
    let mut slug = String::new();
    let mut hash = 0i32;
    for unit in path.encode_utf16() {
        hash = hash.wrapping_mul(31).wrapping_add(i32::from(unit));
        slug.push(if unit < 128 && (unit as u8).is_ascii_alphanumeric() {
            unit as u8 as char
        } else {
            '-'
        });
    }
    if slug.len() <= 200 {
        return slug;
    }
    let mut number = i64::from(hash).unsigned_abs();
    let mut suffix = Vec::new();
    loop {
        suffix.push(b"0123456789abcdefghijklmnopqrstuvwxyz"[(number % 36) as usize] as char);
        number /= 36;
        if number == 0 {
            break;
        }
    }
    format!(
        "{}-{}",
        &slug[..200],
        suffix.iter().rev().collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn project_slug_matches_claude_utf16_and_long_path_rules() {
        assert_eq!(project_slug(Path::new("/work/a_b.c")), "-work-a-b-c");
        assert_eq!(project_slug(Path::new("/work/😀")), "-work---");
        let path = format!("/{}", "a".repeat(201));
        assert_eq!(
            project_slug(Path::new(&path)),
            format!("-{}-85qkr6", "a".repeat(199))
        );
    }
}
