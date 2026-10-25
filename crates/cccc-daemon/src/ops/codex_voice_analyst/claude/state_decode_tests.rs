use super::TranscriptFollower;
use serde_json::Value;
use std::path::PathBuf;

const STATE: &str = include_str!("captured_poll_state.json");
const RECORDS: &str = include_str!("captured_poll_records.jsonl");

struct Capture {
    _temp: tempfile::TempDir,
    config: PathBuf,
    state: PathBuf,
    transcript: PathBuf,
    bytes: Vec<u8>,
    id: String,
}

impl Capture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let config = temp.path().to_path_buf();
        let mut state: Value = serde_json::from_str(STATE).expect("captured state");
        let name = PathBuf::from(state["linkScanPath"].as_str().expect("published path"));
        let name = name.file_name().expect("captured filename");
        let transcript = config.join("projects/source").join(name);
        std::fs::create_dir_all(transcript.parent().expect("parent")).expect("mkdir");
        std::fs::write(&transcript, RECORDS).expect("captured records");
        state["linkScanPath"] = serde_json::json!(transcript);
        let bytes = serde_json::to_vec(&state).expect("relocated state");
        let state = config.join("state.json");
        std::fs::write(&state, &bytes).expect("state");
        let id = transcript
            .file_stem()
            .expect("id")
            .to_str()
            .expect("utf8")
            .to_owned();
        Self {
            _temp: temp,
            config,
            state,
            transcript,
            bytes,
            id,
        }
    }

    fn follower(&self) -> TranscriptFollower {
        TranscriptFollower::new(
            self.state.clone(),
            self.config.clone(),
            self.id.clone(),
            false,
        )
    }
}

#[tokio::test]
async fn identical_captured_state_decodes_once_but_equal_length_edits_are_visible() {
    let capture = Capture::new();
    let mut follower = capture.follower();
    let expected: Vec<Value> = RECORDS
        .lines()
        .map(|l| serde_json::from_str(l).expect("record"))
        .collect();
    assert_eq!(follower.poll().await.expect("first poll"), expected);
    for _ in 0..3 {
        assert!(follower.poll().await.expect("unchanged poll").is_empty());
    }
    assert_eq!(follower.state_decode.parses, 1);
    let mut changed: Value = serde_json::from_slice(&capture.bytes).expect("captured state");
    let other = capture.transcript.with_file_name(format!(
        "{}.jsonl",
        capture.id.chars().rev().collect::<String>()
    ));
    changed["linkScanPath"] = serde_json::json!(other);
    let changed = serde_json::to_vec(&changed).expect("controlled path mutation");
    assert_eq!(changed.len(), capture.bytes.len());
    let modified = std::fs::metadata(&capture.state)
        .expect("metadata")
        .modified()
        .expect("mtime");
    std::fs::write(&capture.state, changed).expect("in-place edit");
    std::fs::File::open(&capture.state)
        .expect("state file")
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .expect("restore mtime");
    assert_eq!(
        follower.poll().await.expect_err("changed identity").kind(),
        std::io::ErrorKind::InvalidData
    );
    assert_eq!(follower.state_decode.parses, 2);
}

#[tokio::test]
async fn cached_path_does_not_hide_transcript_replacement_or_malformed_state() {
    let capture = Capture::new();
    let mut follower = capture.follower();
    follower.poll().await.expect("first poll");
    std::fs::write(&capture.state, &capture.bytes[..capture.bytes.len() / 2])
        .expect("torn state control");
    assert!(
        follower
            .poll()
            .await
            .expect("retry transient state")
            .is_empty()
    );
    std::fs::write(&capture.state, &capture.bytes).expect("restore recorded state");
    assert!(follower.poll().await.expect("restored poll").is_empty());
    assert_eq!(follower.state_decode.parses, 2);
    let replacement = capture.transcript.with_extension("replacement");
    std::fs::write(&replacement, RECORDS).expect("replacement capture");
    std::fs::rename(replacement, &capture.transcript).expect("atomic replacement");
    assert_eq!(
        follower
            .poll()
            .await
            .expect_err("replacement fenced")
            .kind(),
        std::io::ErrorKind::InvalidData
    );
}

#[cfg(unix)]
#[tokio::test]
async fn unchanged_cached_bytes_do_not_bypass_state_symlink_validation() {
    let capture = Capture::new();
    let mut follower = capture.follower();
    follower.poll().await.expect("first poll");
    let original = capture.state.with_extension("original");
    std::fs::rename(&capture.state, &original).expect("move state");
    std::os::unix::fs::symlink(original, &capture.state).expect("symlink control");
    assert_eq!(
        follower.poll().await.expect_err("symlink fenced").kind(),
        std::io::ErrorKind::InvalidData
    );
    assert_eq!(follower.state_decode.parses, 1);
}
