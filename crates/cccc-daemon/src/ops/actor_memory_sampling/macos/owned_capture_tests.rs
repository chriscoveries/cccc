//! Actual Darwin evidence gate, prepared but never executed by this proposal.
//! Opt in only on the separately authorized owned macos-latest runner.
use super::*;
use sha2::{Digest, Sha256};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct OwnedFixture {
    child: Child,
    tree: cccc_runtime::OwnedProcessTree,
}
impl OwnedFixture {
    fn stop(&mut self) -> io::Result<()> {
        self.tree.terminate()?;
        self.child.wait().map(|_| ())
    }
}
impl Drop for OwnedFixture {
    fn drop(&mut self) {
        // A termination error is not permission to block indefinitely in wait.
        // The success path calls stop explicitly so cleanup failure fails proof.
        if let Err(error) = self.stop() {
            eprintln!("owned Darwin footprint fixture cleanup failed: {error}");
        }
    }
}

fn await_marker(fixture: &mut OwnedFixture, path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            fixture
                .tree
                .try_wait(|| fixture.child.try_wait())
                .expect("owned fixture poll")
                .is_none(),
            "fixture exited before marker"
        );
        assert!(
            Instant::now() < deadline,
            "owned allocation marker deadline"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn save_capture(root: &Path, name: &str, output: &cccc_runtime::CapturedOutput) {
    std::fs::write(root.join(format!("{name}.stdout")), &output.stdout).expect("raw stdout");
    std::fs::write(root.join(format!("{name}.stderr")), &output.stderr).expect("raw stderr");
    std::fs::write(root.join(format!("{name}.json")), serde_json::to_vec_pretty(&serde_json::json!({
        "exit_code": output.status.code(), "success": output.status.success(),
        "stdout_truncated": output.stdout_truncated, "stderr_truncated": output.stderr_truncated,
        "stdout_sha256": format!("{:x}", Sha256::digest(&output.stdout)),
        "stderr_sha256": format!("{:x}", Sha256::digest(&output.stderr)),
    })).expect("capture metadata")).expect("metadata file");
}

#[test]
#[ignore = "child process endpoint, invoked only by the authorized owned-capture test"]
fn owned_footprint_child_fixture() {
    let root =
        PathBuf::from(std::env::var_os("T620_MAC_FIXTURE_HOME").expect("private child home"));
    assert!(root.is_absolute() && root.is_dir());
    let mut allocation = vec![0_u8; 32 * 1024 * 1024];
    for index in (0..allocation.len()).step_by(4096) {
        allocation[index] = 1;
    }
    std::hint::black_box(&allocation);
    std::fs::write(root.join("ready"), std::process::id().to_string()).expect("ready");
    for line in std::io::stdin().lock().lines() {
        match line.expect("fixture command").as_str() {
            "grow" => {
                allocation.resize(96 * 1024 * 1024, 0);
                for index in (0..allocation.len()).step_by(4096) {
                    allocation[index] = 2;
                }
                std::hint::black_box(&allocation);
                std::fs::write(root.join("grown"), std::process::id().to_string()).expect("grown");
            }
            "stop" => break,
            _ => panic!("unknown owned fixture command"),
        }
    }
    std::hint::black_box(allocation);
}

#[test]
#[ignore = "requires separately authorized actual macOS vmmap/owned-process validation"]
fn actual_owned_process_footprint_capture_grows_and_keeps_raw_evidence() {
    let temp = tempfile::tempdir().expect("private owned footprint fixture");
    let evidence = PathBuf::from(
        std::env::var_os("T620_MAC_FOOTPRINT_EVIDENCE")
            .expect("an explicit retained evidence path is required"),
    );
    assert!(
        evidence.is_absolute(),
        "explicit evidence path must be absolute"
    );
    // Fail closed if a destination already exists. Never overwrite old proof.
    std::fs::create_dir(&evidence).expect("new evidence directory");
    let mut fixture_command = Command::new(std::env::current_exe().expect("test binary"));
    fixture_command
        .args([
            "--ignored",
            "--exact",
            "ops::actor_memory_sampling::macos::owned_capture_tests::owned_footprint_child_fixture",
            "--nocapture",
        ])
        .env("T620_MAC_FIXTURE_HOME", temp.path())
        .env("HOME", temp.path())
        .env("CCCC_HOME", temp.path().join("cccc"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let (child, tree) = cccc_runtime::OwnedProcessTree::spawn(&mut fixture_command)
        .expect("owned resident allocation child");
    let mut fixture = OwnedFixture { child, tree };
    await_marker(&mut fixture, &temp.path().join("ready"));
    let pid = i32::try_from(fixture.child.id()).expect("owned PID");
    let initial =
        super::super::super::local_headless::run_managed_launch(
            async move { capture_vmmap(pid).await },
        )
        .expect("actual initial vmmap capture");
    save_capture(&evidence, "initial", &initial);
    let before = parse_successful_capture(
        &initial.stdout,
        &initial.stderr,
        initial.status.success(),
        initial.stdout_truncated,
        initial.stderr_truncated,
    )
    .expect("real initial grammar");
    fixture
        .child
        .stdin
        .as_mut()
        .expect("fixture stdin")
        .write_all(b"grow\n")
        .expect("grow command");
    await_marker(&mut fixture, &temp.path().join("grown"));
    let grown =
        super::super::super::local_headless::run_managed_launch(
            async move { capture_vmmap(pid).await },
        )
        .expect("actual grown vmmap capture");
    save_capture(&evidence, "grown", &grown);
    let after = parse_successful_capture(
        &grown.stdout,
        &grown.stderr,
        grown.status.success(),
        grown.stdout_truncated,
        grown.stderr_truncated,
    )
    .expect("real grown grammar");
    let observed_tree = super::super::super::local_headless::run_managed_launch(async move {
        tree_footprint_kib(pid).await
    })
    .expect("actual bounded observed-tree sampler");
    let system = super::super::super::local_headless::run_managed_launch(async move {
        cccc_runtime::capture_command(
            &mut command("/usr/bin/sw_vers"),
            None,
            COMMAND_TIMEOUT,
            VMMAP_LIMIT,
        )
        .await
    })
    .expect("OS provenance");
    save_capture(&evidence, "sw-vers", &system);
    let help = super::super::super::local_headless::run_managed_launch(async move {
        cccc_runtime::capture_command(
            command("/usr/bin/vmmap").arg("-h"),
            None,
            COMMAND_TIMEOUT,
            VMMAP_LIMIT,
        )
        .await
    })
    .expect("vmmap help capture");
    save_capture(&evidence, "vmmap-help", &help);
    std::fs::write(evidence.join("sample.json"), serde_json::to_vec_pretty(&serde_json::json!({
        "basis": super::super::accounting_basis(), "owned_pid": pid,
        "before_kib": before, "after_kib": after, "growth_kib": after.checked_sub(before),
        "tree_kib": observed_tree.0, "tree_counted": observed_tree.1,
        "sampling_model": "non_atomic_observed_process_tree", "sampling_budget_ms": TREE_TIMEOUT.as_millis(),
        "fixture_initial_dirty_mib": 32, "fixture_grown_dirty_mib": 96,
        "source_id": cccc_core::build_info::current()["source_id"],
    })).expect("sample metadata")).expect("sample evidence");
    let still_running = fixture
        .tree
        .try_wait(|| fixture.child.try_wait())
        .expect("owned final poll")
        .is_none();
    fixture
        .stop()
        .expect("owned fixture cleanup is part of successful proof");
    drop(fixture);
    assert!(
        still_running,
        "same owned child must remain alive throughout both samples"
    );
    assert!(
        after >= before + 32 * 1024,
        "a 64MiB increase of dirty owned pages must raise current footprint materially"
    );
    assert_eq!(
        observed_tree.1, 1,
        "fixture observes one genuine owned process"
    );
    assert!(observed_tree.0 > 0);
}
