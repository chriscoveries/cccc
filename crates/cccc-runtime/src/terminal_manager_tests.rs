use crate::terminal_manager::attach_with_snapshot_and_size;
use crate::test_support::{spec, test_guard};
#[cfg(target_os = "linux")]
use crate::write;
use crate::{TerminalAttachMode, attach, attachment_writable, resize_from_attachment, start, stop};
use std::sync::{Arc, Barrier};
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

#[test]
fn attachment_input_enforces_writer_and_session_identity() {
    let _guard = test_guard();
    let temp = tempfile::tempdir().expect("tempdir");
    let group_id = "g_attachment_input";
    let actor_id = "peer1";
    start(spec(&temp, group_id, actor_id, "sleep 30")).expect("first session");

    let control = attach(group_id, actor_id, TerminalAttachMode::Control, false, None)
        .expect("control attachment");
    let viewer = attach(group_id, actor_id, TerminalAttachMode::Viewer, false, None)
        .expect("viewer attachment");
    assert!(control.input().write(b"x").expect("control input"));
    assert!(!viewer.input().write(b"x").expect("viewer input"));

    let stale_input = control.input();
    stop(group_id, actor_id).expect("stop first session");
    start(spec(&temp, group_id, actor_id, "sleep 30")).expect("replacement session");
    assert!(
        !stale_input
            .write(b"stale")
            .expect("stale attachment is rejected")
    );
    stop(group_id, actor_id).expect("cleanup replacement");
}

#[test]
fn attachment_resize_rechecks_writer_after_takeover() {
    let _guard = test_guard();
    let temp = tempfile::tempdir().expect("tempdir");
    let group_id = "g_attachment_resize";
    let actor_id = "peer1";
    start(spec(&temp, group_id, actor_id, "sleep 30")).expect("session");

    let first = attach(group_id, actor_id, TerminalAttachMode::Control, false, None)
        .expect("first controller");
    let takeover = attach(group_id, actor_id, TerminalAttachMode::Control, true, None)
        .expect("takeover controller");

    assert!(
        !resize_from_attachment(group_id, actor_id, first.attachment_id(), 100, 30)
            .expect("stale resize")
    );
    assert!(
        resize_from_attachment(group_id, actor_id, takeover.attachment_id(), 100, 30)
            .expect("writer resize")
    );

    stop(group_id, actor_id).expect("cleanup");
}

#[test]
fn takeover_size_writer_registration_and_snapshot_share_one_operation() {
    let _guard = test_guard();
    let temp = tempfile::tempdir().expect("tempdir");
    let group_id = "g_attachment_snapshot_size";
    let actor_id = "peer1";
    start(spec(&temp, group_id, actor_id, "sleep 30")).expect("session");

    let first = attach(group_id, actor_id, TerminalAttachMode::Control, false, None)
        .expect("first controller");
    let barrier = Arc::new(Barrier::new(3));
    let spawn_takeover = |cols, rows| {
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            attach_with_snapshot_and_size(
                group_id,
                actor_id,
                TerminalAttachMode::Control,
                true,
                None,
                cols,
                rows,
            )
            .expect("sized takeover")
        })
    };
    let first_takeover = spawn_takeover(100, 30);
    let second_takeover = spawn_takeover(120, 40);
    barrier.wait();
    let first_takeover = first_takeover.join().expect("first takeover thread");
    let second_takeover = second_takeover.join().expect("second takeover thread");

    assert_eq!(first_takeover.snapshot_size(), Some((100, 30)));
    assert_eq!(second_takeover.snapshot_size(), Some((120, 40)));
    assert!(first_takeover.terminal_writable());
    assert!(second_takeover.terminal_writable());
    assert_ne!(
        attachment_writable(group_id, actor_id, first_takeover.attachment_id())
            .expect("first takeover status"),
        attachment_writable(group_id, actor_id, second_takeover.attachment_id())
            .expect("second takeover status")
    );
    assert!(!first.input().write(b"stale").expect("replaced controller"));

    stop(group_id, actor_id).expect("cleanup");
}

/// Waits until the session's process runs `cat`, the last exec of each
/// identity test command, so its environment is the one input will meet.
#[cfg(target_os = "linux")]
fn wait_for_cat(group_id: &str, actor_id: &str) {
    let pid = crate::status(group_id, actor_id)
        .expect("status")
        .pid
        .expect("pid");
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default() != "cat\n" {
        assert!(Instant::now() < deadline, "{actor_id} never ran cat");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[cfg(target_os = "linux")]
fn terminal_input_is_refused_unless_the_process_reports_its_actor() {
    let _guard = test_guard();
    let temp = tempfile::tempdir().expect("tempdir");
    let group_id = "g_writer_identity";
    // Each process execs `cat` under a changed identity; any input that
    // reaches it lands in its own file.
    for (actor_id, command, found) in [
        (
            "guarded",
            "exec env CCCC_ACTOR_ID=impostor cat >guarded.received",
            "g_writer_identity/impostor",
        ),
        (
            "unnamed",
            "exec env -u CCCC_ACTOR_ID cat >unnamed.received",
            "no Actor identity",
        ),
    ] {
        start(spec(&temp, group_id, actor_id, command)).expect("session");
        wait_for_cat(group_id, actor_id);
        let control = attach(group_id, actor_id, TerminalAttachMode::Control, false, None)
            .expect("control attachment");
        for (path, result) in [
            (
                "write",
                write(group_id, actor_id, b"typed\n").map(|()| true),
            ),
            ("attachment", control.input().write(b"keys\n")),
        ] {
            let error = match result {
                Err(error) => error.to_string(),
                Ok(accepted) => panic!("{actor_id} via {path}: input not refused ({accepted})"),
            };
            assert_eq!(
                error,
                format!(
                    "runtime session {group_id}/{actor_id} runs a process that reports \
                     {found}; input refused"
                ),
                "{actor_id} via {path}"
            );
        }
        stop(group_id, actor_id).expect("cleanup");
        let received = std::fs::read(temp.path().join(format!("{actor_id}.received")));
        assert_eq!(received.expect("received file"), b"", "{actor_id}");
    }
}

#[test]
#[cfg(target_os = "linux")]
fn terminal_input_reaches_a_process_that_reports_its_actor() {
    let _guard = test_guard();
    let temp = tempfile::tempdir().expect("tempdir");
    let (group_id, actor_id) = ("g_writer_identity_kept", "kept");
    start(spec(&temp, group_id, actor_id, "exec cat >kept.received")).expect("session");
    wait_for_cat(group_id, actor_id);
    let control = attach(group_id, actor_id, TerminalAttachMode::Control, false, None)
        .expect("control attachment");
    write(group_id, actor_id, b"typed\n").expect("write input");
    assert!(control.input().write(b"keys\n").expect("attachment input"));
    let received = temp.path().join("kept.received");
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::fs::read(&received).unwrap_or_default() != b"typed\nkeys\n" {
        assert!(
            Instant::now() < deadline,
            "input never reached its own Actor's process: {:?}",
            std::fs::read_to_string(&received)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    stop(group_id, actor_id).expect("cleanup");
}
