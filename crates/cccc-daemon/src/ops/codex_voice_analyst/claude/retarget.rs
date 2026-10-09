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
    let transcript = find_transcript(&projects, &workspace, session_id)?;
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
    let mut latest_cwd = None;
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
            latest_cwd = Some(PathBuf::from(path));
        }
    }
    let residence_matches = transcript.parent().and_then(Path::file_name)
        == Some(std::ffi::OsStr::new(&project_slug(&workspace)));
    let latest_matches = latest_cwd
        .as_deref()
        .and_then(|p| p.canonicalize().ok())
        .is_some_and(|p| p == workspace);
    if !residence_matches
        && !latest_matches
        && !native_worktree_matches(&config, session_id, &workspace)?
    {
        return Err(OpError::new(
            "claude_workspace_mismatch",
            "Claude transcript residence, latest cwd and native worktree do not match the launch workspace",
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

/// Prefer the launch project's transcript. A uniquely identified transcript in
/// another contained project may still match its latest cwd or native worktree.
fn find_transcript(
    projects: &Path,
    workspace: &Path,
    session_id: &str,
) -> Result<PathBuf, OpError> {
    let preferred = projects
        .join(project_slug(workspace))
        .join(format!("{session_id}.jsonl"));
    if preferred.symlink_metadata().is_ok() {
        return Ok(preferred);
    }
    let entries = std::fs::read_dir(projects).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            OpError::new(
                "claude_transcript_missing",
                "Claude account has no project transcripts",
            )
        } else {
            OpError::io(error)
        }
    })?;
    let mut found = None;
    for entry in entries {
        let entry = entry.map_err(OpError::io)?;
        if !entry.file_type().map_err(OpError::io)?.is_dir() {
            continue;
        }
        let path = entry.path().join(format!("{session_id}.jsonl"));
        if path.symlink_metadata().is_ok() {
            if found.is_some() {
                return Err(OpError::new(
                    "claude_transcript_invalid",
                    "Session has ambiguous transcript residence",
                ));
            }
            found = Some(path);
        }
    }
    found.ok_or_else(|| {
        OpError::new(
            "claude_transcript_missing",
            "Claude transcript is unavailable in this account's project directories",
        )
    })
}

fn native_worktree_matches(
    config: &Path,
    session_id: &str,
    workspace: &Path,
) -> Result<bool, OpError> {
    let jobs = config.join("jobs");
    let entries = match std::fs::read_dir(&jobs) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(OpError::io(error)),
    };
    let canonical_jobs = jobs.canonicalize().map_err(OpError::io)?;
    for entry in entries {
        let entry = entry.map_err(OpError::io)?;
        if !entry.file_type().map_err(OpError::io)?.is_dir() {
            continue;
        }
        let path = entry.path().join("state.json");
        let metadata = match path.symlink_metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(OpError::io(error)),
        };
        if !metadata.is_file() || metadata.len() > MAX_TRANSCRIPT_LINE_BYTES as u64 {
            continue;
        }
        let canonical = path.canonicalize().map_err(OpError::io)?;
        if canonical != path || !canonical.starts_with(&canonical_jobs) {
            continue;
        }
        let value: Value = cccc_core::fs::read_json(&path).map_err(OpError::io)?;
        if value.get("sessionId").and_then(Value::as_str) != Some(session_id) {
            continue;
        }
        if value
            .get("resumeSessionId")
            .and_then(Value::as_str)
            .is_some_and(|id| id != session_id)
        {
            continue;
        }
        if value
            .get("worktreePath")
            .and_then(Value::as_str)
            .and_then(|p| Path::new(p).canonicalize().ok())
            .is_some_and(|p| p == workspace)
        {
            return Ok(true);
        }
    }
    Ok(false)
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
