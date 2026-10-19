#![cfg(unix)]
//! Public runtime API regressions; no production test seam or activity mutation.
use cccc_contracts::RunnerKind;
use cccc_runtime::{ActivitySnapshot, LaunchSpec, TerminalAttachMode};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

struct OwnedTerminal {
    temp: tempfile::TempDir,
    group: String,
    actor: &'static str,
}

impl OwnedTerminal {
    fn start(body: &str) -> Self {
        let temp = tempfile::tempdir().expect("private PTY input fixture");
        let group = format!(
            "input-activity-{}",
            temp.path()
                .file_name()
                .expect("private unique name")
                .to_string_lossy()
        );
        let fixture = Self {
            temp,
            group,
            actor: "actor",
        };
        cccc_runtime::start(fixture.spec(body)).expect("owned shell PTY");
        fixture.wait_file("ready");
        fixture
    }

    fn spec(&self, body: &str) -> LaunchSpec {
        let private_home = self.temp.path().join("provider-home");
        std::fs::create_dir_all(&private_home).expect("private provider home");
        LaunchSpec {
            group_id: self.group.clone(),
            actor_id: self.actor.into(),
            runner: RunnerKind::Pty,
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                format!("stty raw -echo; touch ready; {body}"),
            ],
            cwd: self.temp.path().into(),
            env: BTreeMap::from([
                ("HOME".into(), private_home.to_string_lossy().into_owned()),
                (
                    "CCCC_HOME".into(),
                    self.temp.path().join("cccc").to_string_lossy().into_owned(),
                ),
                (
                    "CODEX_HOME".into(),
                    private_home.join("codex").to_string_lossy().into_owned(),
                ),
                (
                    "CLAUDE_CONFIG_DIR".into(),
                    private_home.join("claude").to_string_lossy().into_owned(),
                ),
            ]),
            cols: 80,
            rows: 24,
        }
    }

    fn snapshot(&self) -> ActivitySnapshot {
        cccc_runtime::activity(&self.group, self.actor).expect("same owned session activity")
    }

    fn wait_file(&self, name: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.temp.path().join(name).exists() {
            assert!(Instant::now() < deadline, "fixture marker {name}");
            assert!(
                cccc_runtime::status(&self.group, self.actor)
                    .expect("owned status")
                    .running
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[cfg(target_os = "linux")]
    fn wait_history(&self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let page = cccc_runtime::history(&self.group, self.actor, None, 4096).expect("history");
            if page.data.contains(needle) {
                return;
            }
            assert!(Instant::now() < deadline, "provider output {needle}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn restart(&self) {
        cccc_runtime::stop(&self.group, self.actor).expect("stop original generation");
        std::fs::remove_file(self.temp.path().join("ready")).expect("remove old marker");
        cccc_runtime::start(self.spec("sleep 30")).expect("replacement generation");
        self.wait_file("ready");
    }
}

impl Drop for OwnedTerminal {
    fn drop(&mut self) {
        // Observe a natural exit before stop so cleanup keeps its real status.
        let _ = cccc_runtime::status(&self.group, self.actor);
        let _ = cccc_runtime::stop(&self.group, self.actor);
    }
}

#[test]
fn raw_runtime_write_records_input_without_waiting_for_delivery() {
    let fixture = OwnedTerminal::start("sleep 30");
    assert!(fixture.snapshot().last_input.is_none());
    cccc_runtime::write(&fixture.group, fixture.actor, b"x").expect("raw write");
    assert!(
        fixture.snapshot().last_input.is_some(),
        "raw terminal bytes are activity"
    );
}

#[test]
fn attached_keystrokes_record_input_in_the_same_owned_session() {
    let fixture = OwnedTerminal::start("sleep 30");
    let attachment = cccc_runtime::attach(
        &fixture.group,
        fixture.actor,
        TerminalAttachMode::Control,
        false,
        None,
    )
    .expect("owned control attachment");
    let before = cccc_runtime::status(&fixture.group, fixture.actor).expect("status");
    assert!(attachment.input().write(b"x").expect("raw keystroke"));
    let after = cccc_runtime::status(&fixture.group, fixture.actor).expect("status");
    assert_eq!(after.pid, before.pid);
    assert_eq!(after.started_at, before.started_at);
    assert!(
        fixture.snapshot().last_input.is_some(),
        "attachment bytes are activity"
    );
}

#[test]
fn empty_or_pre_cancelled_input_does_not_invent_activity() {
    let fixture = OwnedTerminal::start("sleep 30");
    cccc_runtime::write(&fixture.group, fixture.actor, b"").expect("empty raw write");
    assert!(
        cccc_runtime::submit_sequence_interruptible(
            &fixture.group,
            fixture.actor,
            b"",
            &[],
            Duration::ZERO,
            Duration::ZERO,
            &AtomicBool::new(false)
        )
        .expect("empty completed sequence")
    );
    assert!(
        !cccc_runtime::submit_interruptible(
            &fixture.group,
            fixture.actor,
            b"must-not-write",
            b"\r",
            Duration::ZERO,
            &AtomicBool::new(true)
        )
        .expect("pre-cancelled sequence")
    );
    assert!(
        fixture.snapshot().last_input.is_none(),
        "zero bytes never reached admission"
    );
}

#[test]
fn rejected_attachments_do_not_record_activity_on_a_replacement() {
    let fixture = OwnedTerminal::start("sleep 30");
    let viewer = cccc_runtime::attach(
        &fixture.group,
        fixture.actor,
        TerminalAttachMode::Viewer,
        false,
        None,
    )
    .expect("read-only attachment");
    assert!(
        !viewer
            .input()
            .write(b"not-a-writer")
            .expect("viewer refusal")
    );
    let controller = cccc_runtime::attach(
        &fixture.group,
        fixture.actor,
        TerminalAttachMode::Control,
        false,
        None,
    )
    .expect("old controller");
    let stale_input = controller.input();
    fixture.restart();
    assert!(
        !stale_input
            .write(b"old-session-input")
            .expect("stale session refusal")
    );
    assert!(
        fixture.snapshot().last_input.is_none(),
        "old writer must not mark new generation"
    );
}

#[test]
fn automatic_terminal_query_reply_is_not_user_input_activity() {
    let fixture = OwnedTerminal::start(
        "printf '\\033[6n'; dd bs=1 count=6 of=query-reply 2>/dev/null; touch replied; sleep 30",
    );
    fixture.wait_file("replied");
    assert_eq!(
        std::fs::read(fixture.temp.path().join("query-reply")).expect("native reply"),
        b"\x1b[1;1R"
    );
    assert!(
        fixture.snapshot().last_input.is_none(),
        "server terminal replies stay excluded"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn controlled_input_is_recorded_before_a_backpressured_write_completes() {
    const PAYLOAD: usize = 1024 * 1024;
    let fixture = OwnedTerminal::start(
        "dd bs=1 count=1 of=first-byte 2>/dev/null; printf early-response; \
         while [ ! -f release ]; do sleep 0.01; done; \
         head -c 1048575 >/dev/null; touch drained; sleep 30",
    );
    let group = fixture.group.clone();
    let worker = std::thread::spawn(move || {
        cccc_runtime::submit_sequence_interruptible(
            &group,
            "actor",
            &vec![b'x'; PAYLOAD],
            &[],
            Duration::ZERO,
            Duration::ZERO,
            &AtomicBool::new(false),
        )
    });
    // Provider consumes just one byte, answers, then refuses to drain the rest.
    // History is published after record_output, so this is an observation barrier.
    fixture.wait_history("early-response");
    let admitted = fixture.snapshot();
    let backpressured = !worker.is_finished();
    std::fs::write(fixture.temp.path().join("release"), b"").expect("release owned reader");
    let result = worker.join().expect("controlled writer");
    let completed = fixture.snapshot();
    assert!(
        backpressured,
        "one byte read cannot drain a 1MiB PTY payload"
    );
    assert!(result.expect("completed sequence"));
    assert!(
        admitted.last_input.is_some(),
        "input must be visible while write is in flight"
    );
    assert!(admitted.output_bytes_since_input >= b"early-response".len() as u64);
    assert_eq!(
        completed.last_input.map(|event| event.at),
        admitted.last_input.map(|event| event.at),
        "delivery completion must not record a second input event"
    );
    assert_eq!(
        completed.output_bytes_since_input, admitted.output_bytes_since_input,
        "completion must preserve the provider's already observed response"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn partial_input_followed_by_pty_error_is_still_recorded_conservatively() {
    let fixture = OwnedTerminal::start("dd bs=1 count=1 of=first-byte 2>/dev/null; exit 0");
    let result = cccc_runtime::submit_sequence_interruptible(
        &fixture.group,
        fixture.actor,
        &vec![b'x'; 1024 * 1024],
        &[],
        Duration::ZERO,
        Duration::ZERO,
        &AtomicBool::new(false),
    );
    let snapshot = fixture.snapshot();
    assert!(
        result.is_err(),
        "closed slave cannot accept the remaining 1MiB payload"
    );
    assert_eq!(
        std::fs::read(fixture.temp.path().join("first-byte")).expect("delivered prefix"),
        b"x"
    );
    assert!(
        snapshot.last_input.is_some(),
        "partial input must not be misclassified as silence"
    );
}
