//! Replay a redacted native chain; mutations below are explicit fault controls.
use super::*;
use crate::pty_input::{PreparedInput, PtyInput, SharedPtyWriter};
use serde_json::{Value, json};
use std::io::{self, Write};
use std::sync::Arc;

#[derive(Deserialize)]
struct Capture {
    session: String,
    created: i64,
    before_tip: i64,
    after_tip: i64,
    before_transcript: Value,
    after_transcript: Value,
    nodes: Vec<Node>,
}

#[derive(Deserialize)]
struct Node {
    node_id: i64,
    parent_node_id: Option<i64>,
    created_at: i64,
    chat_message: Value,
}

struct Replay {
    _directory: tempfile::TempDir,
    binding: Binding,
    cwd: PathBuf,
    capture: Capture,
}

impl Replay {
    fn new() -> Self {
        let capture: Capture = serde_json::from_str(include_str!(
            "../../tests/fixtures/devin-pty/completed.json"
        ))
        .expect("captured native projection");
        let directory = tempfile::tempdir().expect("replay directory");
        let cwd = directory.path().canonicalize().expect("replay cwd");
        let binding = Binding::fixture(cwd.clone(), capture.session.clone());
        let db = Connection::open(cwd.join("sessions.db")).expect("fixture db");
        // Consumer projection of the captured native columns and unique index.
        db.execute_batch(
            "CREATE TABLE sessions(id TEXT PRIMARY KEY, working_directory TEXT,
             created_at INTEGER, last_activity_at INTEGER, main_chain_id INTEGER);
             CREATE TABLE message_nodes(row_id INTEGER PRIMARY KEY, session_id TEXT,
             node_id INTEGER, parent_node_id INTEGER, chat_message TEXT, created_at INTEGER,
             UNIQUE(session_id,node_id));",
        )
        .expect("native schema projection");
        db.execute(
            "INSERT INTO sessions VALUES (?1,?2,?3,?4,?5)",
            rusqlite::params![
                capture.session,
                cwd.to_str().expect("fixture cwd"),
                capture.created,
                capture
                    .nodes
                    .iter()
                    .find(|node| node.node_id == capture.before_tip)
                    .expect("captured baseline")
                    .created_at,
                capture.before_tip
            ],
        )
        .expect("captured session");
        for node in &capture.nodes {
            db.execute(
                "INSERT INTO message_nodes(session_id,node_id,parent_node_id,chat_message,created_at)
                 VALUES (?1,?2,?3,?4,?5)",
                rusqlite::params![
                    capture.session,
                    node.node_id,
                    node.parent_node_id,
                    node.chat_message.to_string(),
                    node.created_at
                ],
            )
            .expect("captured chain node");
        }
        std::fs::create_dir(cwd.join("transcripts")).expect("transcript directory");
        let replay = Self {
            _directory: directory,
            binding,
            cwd,
            capture,
        };
        replay.transcript(&replay.capture.before_transcript);
        replay
    }

    fn database(&self) -> Connection {
        Connection::open(self.cwd.join("sessions.db")).expect("fixture db")
    }

    fn transcript_path(&self) -> PathBuf {
        self.cwd
            .join("transcripts")
            .join(format!("{}.json", self.capture.session))
    }

    fn transcript(&self, value: &Value) {
        std::fs::write(self.transcript_path(), value.to_string()).expect("captured transcript");
    }

    fn tip(&self, node: i64) {
        self.database()
            .execute("UPDATE sessions SET main_chain_id=?1,last_activity_at=(SELECT created_at FROM message_nodes WHERE node_id=?1)", [node])
            .expect("native tip projection");
    }

    fn baseline(&self) -> Anchor {
        let before = read_snapshot(&self.binding, &self.cwd, None).expect("baseline chain");
        let transcript = read_transcript(&self.binding).expect("baseline transcript");
        Anchor {
            tip: before.tip,
            user: before.user,
            step: transcript.steps.last().expect("baseline step").step_id,
            expected: self
                .capture
                .nodes
                .iter()
                .find(|node| {
                    node.chat_message["role"] == "user" && node.node_id > self.capture.before_tip
                })
                .expect("captured next input")
                .chat_message["content"]
                .as_str()
                .expect("input text")
                .to_owned(),
        }
    }

    fn completion(&self) {
        self.tip(self.capture.after_tip);
        self.transcript(&self.capture.after_transcript);
    }

    fn state(&self, anchor: Option<&Anchor>) -> Option<&'static str> {
        let snapshot = read_snapshot(&self.binding, &self.cwd, anchor.and_then(|a| a.tip))?;
        let transcript = read_transcript(&self.binding)?;
        classify(&snapshot, transcript.steps.last()?, anchor)
    }
}

#[test]
fn native_completion_requires_a_new_input_chain_and_transcript_step() {
    let replay = Replay::new();
    let anchor = replay.baseline();
    assert_eq!(replay.state(Some(&anchor)), None);
    replay.completion();
    assert_eq!(replay.state(None), None, "restart has no input authority");
    assert_eq!(replay.state(Some(&anchor)), Some("idle"));
    let same_completion = replay.baseline();
    replay.database().execute(
        "INSERT INTO message_nodes(session_id,node_id,parent_node_id,chat_message,created_at)
         SELECT session_id,?1,node_id,chat_message,created_at+1 FROM message_nodes WHERE node_id=?2",
        [replay.capture.after_tip+1, replay.capture.after_tip],
    ).expect("duplicate completed-message fault projection");
    replay.tip(replay.capture.after_tip + 1);
    assert_eq!(
        replay.state(Some(&same_completion)),
        None,
        "duplicate completion rows or a changed timestamp are not a new turn"
    );
}

#[test]
fn a_lagged_unrelated_user_completion_does_not_match_the_latest_submission() {
    let replay = Replay::new();
    let mut anchor = replay.baseline();
    replay.completion();
    anchor.expected.push_str(" changed input"); // Explicit payload-mismatch fault.
    assert_eq!(replay.state(Some(&anchor)), None);
}

#[test]
fn retries_and_equal_text_cannot_gain_fresh_input_authority() {
    let replay = Replay::new();
    let anchor = replay.baseline();
    // Captured redacted IDs serve only as opaque admission fault controls here,
    // not as a claim that native message IDs are transport source IDs.
    let ids: Vec<String> = replay
        .capture
        .nodes
        .iter()
        .take(3)
        .map(|node| {
            node.chat_message["message_id"]
                .as_str()
                .expect("captured id")
                .to_owned()
        })
        .collect();
    let mut tracked = Tracking::default();
    // Protocol-control suffix is a projection onto captured text; the retained
    // capture predates this delivery marker and does not claim to contain it.
    let first = format!("{}\n\n[CCCC delivery sources: {}]", anchor.expected, ids[0]);
    assert_eq!(
        tracked.admit(first.as_bytes(), &ids[..1]),
        Some(first.clone())
    );
    assert!(tracked.admit(b"changed retry", &ids[..1]).is_none());
    let framed = format!("\u{1b}[200~{first}\u{1b}[201~");
    assert!(
        tracked.admit(framed.as_bytes(), &ids[1..2]).is_none(),
        "old native text cannot name a newer source tuple"
    );
    assert!(
        tracked.admit(b"different text", &ids[1..2]).is_none(),
        "failed attempts burn their IDs before any provider IO"
    );
    tracked.fence = Some(anchor.clone());
    assert!(tracked.admit(anchor.expected.as_bytes(), &[]).is_none());
    assert!(tracked.fence.is_none());
    assert!(
        tracked.admit(b"\x1b[A", &ids[2..]).is_none(),
        "arbitrary terminal controls cannot create a native text join"
    );
}

#[test]
fn admission_bound_never_evicts_old_identities() {
    let replay = Replay::new();
    let anchor = replay.baseline();
    let mut tracked = Tracking::default();
    // Explicit capacity fault expansion of the captured payload; not a live trace.
    for index in 0..4096 {
        let text = format!(
            "{} {index}\n\n[CCCC delivery sources: expanded-{index}]",
            anchor.expected
        );
        assert!(
            tracked
                .admit(text.as_bytes(), &[format!("expanded-{index}")])
                .is_some()
        );
    }
    assert!(
        tracked
            .admit(b"overflow", &["expanded-4096".to_owned()])
            .is_none()
    );
    assert!(
        tracked
            .admit(b"retry after overflow", &["expanded-0".to_owned()])
            .is_none()
    );
}

#[test]
fn native_observation_does_not_modify_the_session_store() {
    let replay = Replay::new();
    let anchor = replay.baseline();
    replay.completion();
    let path = replay.cwd.join("sessions.db");
    let before = std::fs::read(&path).expect("fixture bytes");
    assert_eq!(replay.state(Some(&anchor)), Some("idle"));
    assert_eq!(std::fs::read(path).expect("fixture bytes"), before);
}

#[test]
fn accepted_user_without_a_completion_and_active_tool_work_stay_nonidle() {
    let replay = Replay::new();
    let anchor = replay.baseline();
    replay.completion();
    let user = replay
        .capture
        .nodes
        .iter()
        .find(|node| {
            node.chat_message["role"] == "user" && node.node_id > replay.capture.before_tip
        })
        .expect("captured next user");
    replay.tip(user.node_id);
    assert_eq!(
        replay.state(Some(&anchor)),
        None,
        "refusal/inflight is unknown"
    );
    let tool = replay
        .capture
        .nodes
        .iter()
        .find(|node| {
            node.chat_message["role"] == "tool" && node.node_id > replay.capture.before_tip
        })
        .expect("captured tool result");
    replay.tip(tool.node_id);
    assert_eq!(replay.state(Some(&anchor)), Some("working"));
}

#[test]
fn partial_oversized_foreign_or_unknown_schema_transcripts_are_unknown() {
    let replay = Replay::new();
    let anchor = replay.baseline();
    replay.completion();
    std::fs::write(replay.transcript_path(), b"{").expect("partial rewrite fault");
    assert_eq!(replay.state(Some(&anchor)), None);
    for (key, value) in [
        ("schema_version", "unknown"),
        ("session_id", "other-session"),
    ] {
        let mut corrupted = replay.capture.after_transcript.clone();
        corrupted[key] = json!(value);
        replay.transcript(&corrupted);
        assert_eq!(replay.state(Some(&anchor)), None);
    }
    File::create(replay.transcript_path())
        .expect("oversize fault")
        .set_len(MAX_FILE + 1)
        .expect("oversize mutation");
    assert_eq!(replay.state(Some(&anchor)), None);
}

#[test]
fn a_missing_ancestor_or_replaced_input_epoch_cannot_prove_idle() {
    let replay = Replay::new();
    let anchor = replay.baseline();
    replay.completion();
    let replaced = Anchor {
        tip: Some(i64::MAX),
        user: anchor.user.clone(),
        step: anchor.step,
        expected: anchor.expected.clone(),
    };
    assert_eq!(replay.state(Some(&replaced)), None);
    let parent = replay
        .capture
        .nodes
        .iter()
        .find(|node| node.node_id == replay.capture.after_tip)
        .expect("completion node")
        .parent_node_id;
    replay
        .database()
        .execute("DELETE FROM message_nodes WHERE node_id=?1", [parent])
        .expect("missing ancestor fault");
    assert_eq!(replay.state(Some(&anchor)), None);
}

struct RefusedInput;
impl Write for RefusedInput {
    fn write(&mut self, _data: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("injected input failure"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl PtyInput for RefusedInput {}

#[test]
fn manual_input_paths_retire_old_completion_even_when_the_write_fails() {
    let replay = Replay::new();
    for method in 0..2 {
        // Invalid process is an explicit binding-loss fault, not a fake live actor.
        let observer = Arc::new(Tracker::new(
            0,
            replay.cwd.clone(),
            String::new(),
            String::new(),
            String::new(),
        ));
        observer.state.lock().expect("tracker").fence = Some(replay.baseline());
        let raw: SharedPtyWriter = Arc::new(Mutex::new(Box::new(RefusedInput)));
        let input = PreparedInput::new(raw, Some(observer.clone()));
        match method {
            0 => assert!(input.write(b"\r").is_err()),
            _ => assert!(input.write_interruptibly(b"\r", &|| false).is_err()),
        }
        assert!(observer.state.lock().expect("tracker").fence.is_none());
    }
}
