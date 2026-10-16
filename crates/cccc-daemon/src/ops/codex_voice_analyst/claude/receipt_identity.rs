//! Observe legacy worker/transcript divergence without selecting a conversation.
use super::*;

pub(crate) fn inspect_receipt_identity(
    environment: &BTreeMap<String, String>,
    cwd: &Path,
    worker: &str,
) -> io::Result<Option<Value>> {
    let config = match command::existing_config_dir(environment) {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let jobs = config.join("jobs");
    let entries = match std::fs::read_dir(&jobs) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if jobs.canonicalize()? != jobs {
        return Err(invalid("Claude jobs directory contains a symlink"));
    }
    let mut observations = Vec::new();
    let mut divergent = false;
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(short) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if short.len() != 8 || !short.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        let path = entry.path().join("state.json");
        let metadata = match path.symlink_metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if !metadata.file_type().is_file() || metadata.len() > MAX_STATE_BYTES {
            continue;
        }
        if path.canonicalize()? != path {
            continue;
        }
        let bytes = std::fs::read(&path)?;
        let state: Value = match serde_json::from_slice(&bytes) {
            Ok(state) => state,
            Err(_) => continue, // An unrelated corrupt job establishes no mapping.
        };
        if state["sessionId"].as_str() != Some(worker) {
            continue;
        }
        let transcript = state["resumeSessionId"]
            .as_str()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .unwrap_or(worker);
        // Historical same-session metadata is not a foreign-conversation
        // observation. In particular, native worktree records can predate
        // daemonShort or retain the original cwd. Existing retarget and launch
        // guards still verify the selected session; only a different identity
        // enters the adoption binding checks below.
        if transcript == worker {
            continue;
        }
        // A missing old workspace is relevant only when this is our saved
        // worker. Unrelated jobs must not prevent ordinary drift selecting fresh.
        let workspace = cwd.canonicalize()?;
        if state["daemonShort"].as_str() != Some(short.as_str()) {
            return Err(invalid(
                "Saved Claude worker has conflicting short identity",
            ));
        }
        let saved_cwd = state["cwd"]
            .as_str()
            .ok_or_else(|| invalid("Saved Claude worker has no verifiable working directory"))?;
        if Path::new(saved_cwd).canonicalize()? != workspace {
            return Err(invalid(
                "Saved Claude worker belongs to a different workspace",
            ));
        }
        if uuid::Uuid::parse_str(transcript).is_err() {
            return Err(invalid(
                "Saved Claude worker has an invalid transcript identity",
            ));
        }
        let differs = transcript != worker;
        divergent |= differs;
        let mut observation = json!({
            "session_id":transcript,
            "daemon_short":short,
            "transcript_path":state["linkScanPath"],
            "transcript_validated":false,
        });
        if differs {
            match validate_observed_transcript(&config, cwd, transcript, &state) {
                Ok(()) => observation["transcript_validated"] = json!(true),
                Err(error) => observation["validation_error"] = json!(error.to_string()),
            }
        }
        observations.push(observation);
    }
    if !divergent {
        return Ok(None);
    }
    observations.sort_by(|left, right| {
        left["daemon_short"]
            .as_str()
            .cmp(&right["daemon_short"].as_str())
    });
    Ok(Some(json!({
        "worker_session_id":worker,
        "observed_transcripts":observations,
    })))
}

fn validate_observed_transcript(
    config: &Path,
    cwd: &Path,
    transcript: &str,
    state: &Value,
) -> io::Result<()> {
    let (validated, _) = retarget::inspect_transcript(config, cwd, transcript)
        .map_err(|error| invalid(error.message))?;
    let link = state["linkScanPath"]
        .as_str()
        .filter(|path| !path.is_empty())
        .ok_or_else(|| invalid("Saved Claude worker has no published transcript path"))?;
    if Path::new(link) != validated {
        return Err(invalid(
            "Published Claude transcript differs from validated history",
        ));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
