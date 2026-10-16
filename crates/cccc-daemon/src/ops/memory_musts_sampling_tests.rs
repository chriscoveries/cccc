//! Tests-only proposals v4: MUST2a members, tightened guards (corrects v3).
//! v1/v2/v3 sampling files are PRESERVED IMMUTABLY; behavior is unchanged
//! from v3 — this revision removes the unused glob import and qualifies the
//! Drop/self-expiry docs per review (kill-failure and zombie-reaping limits).
//!
//! v1 (`../tests-v1/sampling_member_tests.rs`) and the v2 sampling file are
//! PRESERVED IMMUTABLY. This file fixes: leaf self-expiry (no `while True`),
//! `Drop` that never blocks (kill + bounded `try_wait` polling, abandon past
//! deadline), `reap_exited` retaining ownership on wait error, explicit
//! finish/reap with asserted cleanup outcomes BEFORE evidence assertions,
//! and a literal allocation floor (no second volatile sample).
//! Wiring: same as v1/v2 (`sampling.rs` `#[path] mod`, linux-gated).
//!
//! RED/GREEN vs immutable 4881: `vanished_*` / `zombie_*` MUST FAIL on 4881
//! (whole-union `(0,0)`); `empty_and_absurd_*` MUST PASS on 4881 and after.
//! All tests drive the REAL `roots_rss_kib` with live PIDs.

// NOTE: no `use super::*` — every production call below is an explicit
// `super::roots_rss_kib` path, so a future strict Clippy has no unused
// glob to bill the fixture for.
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// Known touched allocation floor: the leaf holds 32 MiB of touched pages.
/// A literal, never a second sample.
const LEAF_FLOOR_KIB: u64 = 32 * 1024;
const FIXTURE_TIMEOUT: Duration = Duration::from_secs(10);

/// Owns at most one fixture child. `Drop` is bounded best-effort for unwind
/// paths ONLY: it signals, polls `try_wait` (never blocking `wait`) up to a
/// short deadline, then abandons. Kill failure proves NOTHING by itself (a
/// failed signal only means undelivered, not gone) — the brief poll after it
/// is the actual reaping attempt. Abandonment bounds only the wait, not the
/// process: the fixture program's finite self-expiry ends the LIVE phase
/// (CPU loop), while an exited-but-unreaped zombie still needs a reaper
/// (parent guard, or init after reparenting) — self-expiry does not reap.
/// The 180 s bound covers the live phase only. Reaping after an abandoned
/// guard remains a separate residual, not a guaranteed outcome of self-expiry.
struct ReapedOnDrop(Option<Child>);

impl ReapedOnDrop {
    fn pid(&self) -> u32 {
        self.0.as_ref().expect("child still owned").id()
    }
}

impl Drop for ReapedOnDrop {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            // A failed kill proves nothing about liveness. Polling remains
            // bounded whether signaling succeeded or failed.
            let _ = child.kill();
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                match child.try_wait() {
                    Ok(Some(_)) => return, // reaped
                    Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                    Err(_) => return, // handle lost: abandon, never block
                }
            }
            // Past deadline with a live child: abandon (documented). The
            // fixture program's own self-expiry bounds the leak.
        }
    }
}

/// Reap an ALREADY-EXITED child, asserting the cleanup outcome. Polls
/// `try_wait` (bounded, non-blocking); on wait error the guard KEEPS
/// ownership (no take) so `Drop` gets its retry instead of leaking the
/// handle. Returns the pid for measurement. Panics loudly on misuse — a
/// cleanup failure must never masquerade as a policy signal.
fn reap_exited(guard: &mut ReapedOnDrop, what: &str) -> u32 {
    let pid = guard.pid();
    let deadline = Instant::now() + FIXTURE_TIMEOUT;
    loop {
        match guard.0.as_mut().expect("owned").try_wait() {
            Ok(Some(status)) => {
                assert!(status.success(), "{what}: fixture child failed");
                guard.0.take();
                return pid;
            }
            Ok(None) => {}
            Err(error) => {
                panic!("{what}: wait error, ownership retained for Drop: {error:?}");
            }
        }
        assert!(
            Instant::now() < deadline,
            "{what}: fixture child never exited"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().expect("private fixture directory")
}

/// Spawn a bounded LEAF fixture: one python process holding a 32 MiB touched
/// bytearray, signalling readiness, then idling with a FINITE self-expiry
/// (180 s) independent of the Rust guard — no infinite `while True`. It
/// spawns no children of its own, so its `tree_pids` subtree is exactly
/// itself (unlike the test binary's own PID, whose subtree contains every
/// live fixture child of the parallel suite). Returns the live guard plus
/// the release-file path for explicit finish.
fn spawn_leaf(dir: &std::path::Path) -> (ReapedOnDrop, std::path::PathBuf) {
    let ready = dir.join("leaf-ready");
    let release = dir.join("leaf-release");
    let program = r#"
import pathlib, sys, time
ready, release = sys.argv[1], sys.argv[2]
keep = bytearray(32 * 1024 * 1024)
for offset in range(0, len(keep), 4096):
    keep[offset] = 1
pathlib.Path(ready).write_text("ready")
deadline = time.monotonic() + 180
while time.monotonic() < deadline and not pathlib.Path(release).exists():
    time.sleep(0.05)
"#;
    let mut guard = ReapedOnDrop(Some(
        Command::new("python3")
            .arg("-u")
            .arg("-c")
            .arg(program)
            .arg(&ready)
            .arg(&release)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn bounded leaf fixture"),
    ));
    let deadline = Instant::now() + FIXTURE_TIMEOUT;
    loop {
        if ready.exists() {
            return (guard, release);
        }
        match guard.0.as_mut().expect("live").try_wait() {
            Ok(None) => {}
            other => panic!("leaf fixture exited before readiness: {other:?}"),
        }
        assert!(Instant::now() < deadline, "leaf never became ready");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Explicit finish: release the leaf, reap with asserted outcome. Called
/// BEFORE evidence assertions so a cleanup failure fails as such.
fn finish_leaf(guard: &mut ReapedOnDrop, release: &std::path::Path, what: &str) {
    std::fs::write(release, b"release").expect("release leaf");
    reap_exited(guard, what);
}

#[test]
fn vanished_member_does_not_zero_readable_union_red_on_4881() {
    let dir = tempdir();
    let (mut leaf, leaf_release) = spawn_leaf(dir.path());
    let mut dead = ReapedOnDrop(Some(
        Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .expect("spawn"),
    ));
    let dead_pid = reap_exited(&mut dead, "vanished child");
    // Absence is OBSERVED (NotFound), not assumed: any other error kind
    // (e.g. permission) belongs to the unavailable class, not the vanished one.
    match std::fs::read_to_string(format!("/proc/{dead_pid}/status")) {
        Err(error) => assert_eq!(
            error.kind(),
            std::io::ErrorKind::NotFound,
            "reaped child must be absent, got: {error:?}"
        ),
        Ok(_) => panic!("reaped child unexpectedly present (pid reuse across test)"),
    }
    // Capture while both fixtures are in their measured states, finish with
    // asserted cleanup, THEN assert evidence.
    let (total, counted) = super::roots_rss_kib(&[leaf.pid(), dead_pid]);
    finish_leaf(&mut leaf, &leaf_release, "leaf");
    assert_eq!(counted, 1, "readable leaf counted, vanished member skipped");
    assert!(
        total >= LEAF_FLOOR_KIB,
        "union must hold the known floor (got {total})"
    );
}

#[test]
fn zombie_member_counts_zero_not_unreadable_red_on_4881() {
    let dir = tempdir();
    let (mut leaf, leaf_release) = spawn_leaf(dir.path());
    let mut zombie = ReapedOnDrop(Some(
        Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .expect("spawn"),
    ));
    // The child has exited but is unreaped: poll for the kernel zombie state
    // (bounded), then measure while it is still unreaped.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let body = std::fs::read_to_string(format!("/proc/{}/status", zombie.pid()))
            .expect("unreaped child status stays readable");
        if body
            .lines()
            .any(|line| line.starts_with("State:") && line.contains("(zombie)"))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "child never reached zombie state"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let (total, counted) = super::roots_rss_kib(&[leaf.pid(), zombie.pid()]);
    // Explicit reap with asserted outcome BEFORE evidence assertions; the
    // narrowed contract needs the zombie unreaped only during measurement.
    let zombie_pid = zombie.pid();
    assert_eq!(reap_exited(&mut zombie, "zombie"), zombie_pid);
    finish_leaf(&mut leaf, &leaf_release, "leaf");
    // Narrowed contract: a positively-known zombie contributes +0 and is NOT
    // counted as sampled; the remainder sum is complete, not partial. Pinned
    // separately (counts vs floor) — sampling is explicitly non-atomic, so no
    // exact-equality comparison between passes.
    assert_eq!(
        counted, 1,
        "zombie must not count as sampled (got {counted})"
    );
    assert!(
        total >= LEAF_FLOOR_KIB,
        "union must hold the known floor (got {total})"
    );
}

#[test]
fn empty_and_absurd_roots_stay_unreadable_preserved() {
    // Unknown-member contract unchanged: green on 4881 AND fixed. (Also
    // covered by `observation_roots_reject_empty_init_overflow_and_unbounded_inputs`.)
    assert_eq!(super::roots_rss_kib(&[]), (0, 0));
    assert_eq!(super::roots_rss_kib(&[0]), (0, 0));
    assert_eq!(super::roots_rss_kib(&[1]), (0, 0));
    assert_eq!(super::roots_rss_kib(&vec![7u32; 129]), (0, 0));
}
