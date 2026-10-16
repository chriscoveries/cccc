//! Deterministic receipt RMW interleavings; no provider processes or global hooks.
use super::*;
use std::cell::RefCell;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

struct BeforeLock {
    receipt: PathBuf,
    reached: Sender<()>,
    release: Receiver<()>,
}

thread_local! {
    static BEFORE_LOCK: RefCell<Option<BeforeLock>> = const { RefCell::new(None) };
}

// This checkpoint is immediately before lock admission, so the caller must not
// have loaded its mutable receipt snapshot yet. It is one-shot and thread-local.
pub(super) fn before_lock(receipt: &Path) -> std::io::Result<()> {
    let checkpoint = BEFORE_LOCK.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot
            .as_ref()
            .is_some_and(|checkpoint| checkpoint.receipt == receipt)
        {
            slot.take()
        } else {
            None
        }
    });
    if let Some(checkpoint) = checkpoint {
        checkpoint.reached.send(()).map_err(std::io::Error::other)?;
        checkpoint
            .release
            .recv_timeout(Duration::from_secs(30))
            .map_err(std::io::Error::other)?;
    }
    Ok(())
}

fn fixture() -> (tempfile::TempDir, HomeLayout, String, PathBuf, Vec<String>) {
    let temp = tempfile::tempdir().expect("private receipt-lock fixture");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    let store = GroupStore::new(home.clone()).expect("store");
    let group = store.create("receipt-lock race", "").expect("group");
    let cwd = temp.path().join("workspace");
    std::fs::create_dir(&cwd).expect("workspace");
    (temp, home, group.group_id, cwd, vec!["claude".into()])
}

fn paused_observer(
    home: &HomeLayout,
    group: &str,
) -> (
    Receiver<()>,
    Sender<()>,
    std::thread::JoinHandle<std::io::Result<()>>,
) {
    let receipt = path(home, group, "actor").expect("receipt path");
    let (reached, ready) = channel();
    let (release, released) = channel();
    let home = home.clone();
    let group = group.to_owned();
    let observer = std::thread::spawn(move || {
        BEFORE_LOCK.with(|slot| {
            *slot.borrow_mut() = Some(BeforeLock {
                receipt,
                reached,
                release: released,
            });
        });
        recovery_decision(&home, &group, "actor", "skip", "already_running")
    });
    (ready, release, observer)
}

#[test]
fn queued_observation_keeps_the_current_session_and_exact_new_attempt() {
    const OLD: &str = "52b41c61-e23c-4b7c-8b60-809c347451b5";
    const CURRENT: &str = "52b41c61-e23c-4b7c-8b60-809c347451b6";
    let (_temp, home, group, cwd, command) = fixture();
    let env = std::collections::BTreeMap::new();
    record_claude_managed_session(&home, &group, "actor", &cwd, &command, &env, OLD, false)
        .expect("old binding");
    let old_attempt = prepare_claude_managed_session(&home, &group, "actor", &cwd, &command, &env)
        .expect("prepare old")
        .expect("old attempt");

    let (ready, release, observer) = paused_observer(&home, &group);
    ready
        .recv_timeout(Duration::from_secs(30))
        .expect("observer reached common lock boundary");
    // Observer is blocked before admission. Both real authoritative mutators
    // execute while it is paused, and must be read only after its later lock.
    record_claude_managed_session(&home, &group, "actor", &cwd, &command, &env, CURRENT, false)
        .expect("replacement binding");
    let current = prepare_claude_managed_session(&home, &group, "actor", &cwd, &command, &env)
        .expect("prepare current")
        .expect("current attempt");
    assert_eq!(current.session_id, CURRENT);
    assert_ne!(current.attempt_id, old_attempt.attempt_id);
    let mut expected = read(&home, &group, "actor").expect("current receipt");

    release.send(()).expect("release observer");
    observer
        .join()
        .expect("observer thread")
        .expect("optional observation");
    let mut actual = read(&home, &group, "actor").expect("receipt after observation");
    assert_eq!(actual["provider_session_id"], CURRENT);
    assert_eq!(actual["last_resume_attempt_id"], current.attempt_id);
    assert_eq!(actual["status"], "usable");
    assert_eq!(actual["failure_count"], 0);
    let timestamp = actual["updated_at"].as_str().expect("diagnostic timestamp");
    chrono::DateTime::parse_from_rfc3339(timestamp).expect("valid timestamp");
    assert_eq!(
        actual["last_recovery_decision"],
        json!({"action":"skip", "reason":"already_running", "session_id":CURRENT, "at":timestamp}),
        "observation belongs to the current binding, never the earlier saved session"
    );
    for document in [&mut expected, &mut actual] {
        document.remove("updated_at");
        document.remove("last_recovery_decision");
    }
    assert_eq!(
        actual, expected,
        "no attempt, archive, identity or eligibility field may be lost"
    );
}

#[test]
fn queued_observation_does_not_resurrect_a_removed_receipt() {
    const SAVED: &str = "52b41c61-e23c-4b7c-8b60-809c347451b5";
    let (_temp, home, group, cwd, command) = fixture();
    record_claude_managed_session(
        &home,
        &group,
        "actor",
        &cwd,
        &command,
        &std::collections::BTreeMap::new(),
        SAVED,
        false,
    )
    .expect("saved receipt");
    let receipt = path(&home, &group, "actor").expect("receipt path");
    let (ready, release, observer) = paused_observer(&home, &group);
    ready
        .recv_timeout(Duration::from_secs(30))
        .expect("observer reached common lock boundary");
    remove(&home, &group, "actor").expect("retire current receipt");
    assert!(!receipt.exists());
    release.send(()).expect("release observer");
    observer
        .join()
        .expect("observer thread")
        .expect("optional observation");
    assert!(
        !receipt.exists(),
        "a late observation must not recreate retired session state"
    );
}

thread_local! {
    static FAIL_WRITE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

pub(super) fn fail_next_write(receipt: &Path) {
    FAIL_WRITE.with(|slot| *slot.borrow_mut() = Some(receipt.to_path_buf()));
}

pub(super) fn before_write(receipt: &Path) -> std::io::Result<()> {
    let fail = FAIL_WRITE.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_deref() == Some(receipt) {
            slot.take();
            true
        } else {
            false
        }
    });
    if fail {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "fixture mandatory receipt write denied",
        ))
    } else {
        Ok(())
    }
}
