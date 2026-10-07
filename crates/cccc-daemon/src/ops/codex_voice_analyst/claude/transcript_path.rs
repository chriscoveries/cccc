//! Locate retained Agent View transcripts without changing session identity.
use std::io;
use std::path::{Path, PathBuf};

/// Find a unique session file after a move. The follower separately verifies
/// file containment and consumed history before adopting a different path.
pub(super) async fn recover_missing(
    config_dir: &Path,
    session_id: &str,
    published: PathBuf,
    current: Option<&Path>,
    stale: &mut Option<PathBuf>,
    resuming: bool,
) -> io::Result<PathBuf> {
    match tokio::fs::symlink_metadata(&published).await {
        Ok(_)
            if stale.as_ref() == Some(&published)
                && current != Some(published.as_path())
                && current.is_some_and(|path| path.exists()) =>
        {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Claude's stale transcript path reappeared while the session was active",
            ))
        }
        Ok(_) => Ok(published),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if stale.as_ref() == Some(&published)
                && let Some(current) = current
                && tokio::fs::symlink_metadata(current).await.is_ok()
            {
                return Ok(current.to_path_buf());
            }
            if !resuming && current.is_none() {
                return Err(io::Error::new(
                    error.kind(),
                    format!("Claude transcript is missing: {}", published.display()),
                ));
            }
            let found = find(config_dir, session_id)?.ok_or_else(|| io::Error::new(
                io::ErrorKind::NotFound,
                format!("Claude transcript {} is missing and no retained transcript exists for session {session_id}", published.display()),
            ))?;
            *stale = Some(published);
            Ok(found)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
#[path = "transcript_relocation_tests.rs"]
mod relocation_tests;

pub(super) fn find(config_dir: &Path, session_id: &str) -> io::Result<Option<PathBuf>> {
    super::validate_session_id(session_id)?;
    let projects = config_dir.join("projects");
    let entries = match std::fs::read_dir(&projects) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut found = None;
    for entry in entries {
        let entry = entry?;
        // Never traverse symlinked project directories.
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path().join(format!("{session_id}.jsonl"));
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {
                if found.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Claude session has multiple transcript candidates",
                    ));
                }
                // The follower validates the file type, containment and identity.
                found = Some(path);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::super::TranscriptFollower;
    use super::*;
    use serde_json::json;
    use std::io::Write;

    const ID: &str = "52b41c61-e23c-4b7c-8b60-809c347451b5";

    #[tokio::test]
    async fn resumes_unpublished_metadata_and_message_transcripts_without_replaying_history() {
        for previous in [
            json!({"type":"custom-title"}),
            json!({"type":"user","message":{"content":"old"}}),
        ] {
            let temp = tempfile::tempdir().expect("create test directory");
            let project = temp.path().join("projects/workspace");
            std::fs::create_dir_all(&project).expect("create fixture directory");
            let path = project.join(format!("{ID}.jsonl"));
            std::fs::write(&path, format!("{previous}\n")).expect("write test fixture");
            let state = temp.path().join("state.json");
            std::fs::write(&state, "{}").expect("write test fixture");
            let mut follower =
                TranscriptFollower::new(state.clone(), temp.path().into(), ID.into(), true);
            follower
                .initialize()
                .await
                .expect("complete initialize in fixture");
            assert!(
                follower
                    .poll()
                    .await
                    .expect("complete poll in fixture")
                    .is_empty()
            );
            let next = json!({"type":"user","message":{"content":"new"}});
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("complete open in fixture");
            writeln!(file, "{next}").expect("fixture value must be available");
            assert_eq!(
                follower.poll().await.expect("complete poll in fixture"),
                vec![next]
            );
            // Later publication of the same file keeps the original read offset.
            std::fs::write(&state, json!({"linkScanPath":path}).to_string())
                .expect("write test fixture");
            assert!(
                follower
                    .poll()
                    .await
                    .expect("complete poll in fixture")
                    .is_empty()
            );
        }
    }

    #[tokio::test]
    async fn fresh_publication_waits_for_the_first_transcript_without_losing_input() {
        let temp = tempfile::tempdir().expect("fixture");
        let project = temp.path().join("projects/workspace");
        std::fs::create_dir_all(&project).expect("project");
        let path = project.join(format!("{ID}.jsonl"));
        let state = temp.path().join("state.json");
        std::fs::write(&state, json!({"linkScanPath":path}).to_string()).expect("state");
        let mut follower = TranscriptFollower::new(state, temp.path().into(), ID.into(), false);
        follower
            .initialize()
            .await
            .expect("publication may precede creation");
        assert!(
            follower
                .poll()
                .await
                .expect("bounded publication wait")
                .is_empty()
        );
        let record = json!({"type":"user","sessionId":ID,"promptId":"first", "message":{"content":"first input"}});
        std::fs::write(&path, format!("{record}\n")).expect("transcript");
        assert_eq!(follower.poll().await.expect("first input"), vec![record]);
        assert!(follower.poll().await.expect("no replay").is_empty());
    }

    #[tokio::test]
    async fn first_transcript_publication_wait_is_bounded() {
        let temp = tempfile::tempdir().expect("fixture");
        let state = temp.path().join("state.json");
        let path = temp
            .path()
            .join("projects/workspace")
            .join(format!("{ID}.jsonl"));
        std::fs::write(&state, json!({"linkScanPath":path}).to_string()).expect("state");
        let mut follower = TranscriptFollower::new(state, temp.path().into(), ID.into(), false);
        follower.initialize().await.expect("start publication wait");
        tokio::time::sleep(super::super::transcript_continuity::RELOCATION_TIMEOUT).await;
        let error = follower.poll().await.expect_err("permanent absence fails");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn refuses_ambiguous_sessions_and_does_not_select_other_sessions() {
        let temp = tempfile::tempdir().expect("create test directory");
        for project in ["one", "two"] {
            let dir = temp.path().join("projects").join(project);
            std::fs::create_dir_all(&dir).expect("create fixture directory");
            std::fs::write(dir.join(format!("{ID}.jsonl")), "{}\n").expect("write test fixture");
        }
        assert_eq!(
            find(temp.path(), ID)
                .expect_err("operation must fail in this scenario")
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert!(
            find(temp.path(), "62b41c61-e23c-4b7c-8b60-809c347451b5")
                .expect("complete find in fixture")
                .is_none()
        );
    }
    #[tokio::test]
    async fn mismatched_transcript_diagnostic_identifies_the_job_and_both_sessions() {
        let temp = tempfile::tempdir().expect("fixture");
        let job = temp.path().join("jobs/abcdef12");
        std::fs::create_dir_all(&job).expect("job");
        let state = job.join("state.json");
        let observed = "62b41c61-e23c-4b7c-8b60-809c347451b5.jsonl";
        let path = temp.path().join("projects/workspace").join(observed);
        std::fs::write(&state, json!({"linkScanPath":path}).to_string()).expect("state");
        let mut follower = TranscriptFollower::new(state, temp.path().into(), ID.into(), false);
        let error = follower
            .initialize()
            .await
            .expect_err("foreign identity remains rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let detail = error.to_string();
        assert!(
            detail.contains(ID),
            "expected session is attributable: {detail}"
        );
        assert!(
            detail.contains(observed),
            "observed filename is attributable: {detail}"
        );
        assert!(
            detail.contains("abcdef12"),
            "managed job is attributable: {detail}"
        );
        assert!(
            follower.path.is_none(),
            "foreign transcript was not adopted"
        );
    }
}
