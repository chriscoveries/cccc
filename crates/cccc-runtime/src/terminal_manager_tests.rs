use crate::terminal_manager::attach_with_snapshot_and_size;
use crate::test_support::{spec, test_guard};
use crate::{
    RuntimeError, TerminalAttachMode, attach, attachment_writable, resize_from_attachment, start,
    status, stop,
};
use std::sync::{Arc, Barrier};
use std::time::Duration;

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

/// A terminal launched for one actor must refuse another actor's keystrokes.
/// The launcher pins CCCC_ACTOR_ID last, so a process that re-execs under a
/// different id (the shape of the JAMES incident, where a lane ran for two
/// hours as the peer it was never launched for) is detectable before the
/// write. The attachment path is the human's keystrokes, so a silent
/// acceptance is the human typing into a lane that works as someone else.
#[test]
#[cfg(target_os = "linux")]
fn attachment_refuses_keystrokes_into_a_terminal_running_as_another_actor() {
    let _guard = test_guard();
    let temp = tempfile::tempdir().expect("tempdir");
    let group_id = "g_attachment_identity";
    let actor_id = "guarded";
    start(spec(
        &temp,
        group_id,
        actor_id,
        "exec env CCCC_ACTOR_ID=impostor cat >/dev/null",
    ))
    .expect("terminal");
    // Wait for the re-exec: /proc shows the new environment only after exec.
    let pid = status(group_id, actor_id)
        .expect("status")
        .pid
        .expect("pid");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut seen = String::new();
    while std::time::Instant::now() < deadline {
        if let Ok(bytes) = std::fs::read(format!("/proc/{pid}/environ")) {
            seen = String::from_utf8_lossy(&bytes).into_owned();
            if seen.contains("CCCC_ACTOR_ID=impostor") {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        seen.contains("CCCC_ACTOR_ID=impostor"),
        "fixture env: {seen}"
    );
    let control = attach(group_id, actor_id, TerminalAttachMode::Control, false, None)
        .expect("control attachment");
    match control.input().write(b"task for the wrong lane\n") {
        Err(RuntimeError::IdentityMismatch {
            actor_id,
            found_actor_id,
            ..
        }) => {
            assert_eq!(actor_id, "guarded");
            assert_eq!(found_actor_id, "impostor");
        }
        other => panic!("expected IdentityMismatch, got {other:?}"),
    }
    stop(group_id, actor_id).expect("cleanup");
}

/// The same terminal, launched under its own id, still accepts the
/// keystrokes: the fence refuses a mis-keyed process, not every attachment.
#[test]
#[cfg(target_os = "linux")]
fn attachment_accepts_keystrokes_into_a_terminal_running_as_its_own_actor() {
    let _guard = test_guard();
    let temp = tempfile::tempdir().expect("tempdir");
    let group_id = "g_attachment_identity_ok";
    let actor_id = "faithful";
    start(spec(&temp, group_id, actor_id, "cat >/dev/null")).expect("terminal");
    let control = attach(group_id, actor_id, TerminalAttachMode::Control, false, None)
        .expect("control attachment");
    assert!(control.input().write(b"task\n").expect("keystrokes"));
    stop(group_id, actor_id).expect("cleanup");
}
