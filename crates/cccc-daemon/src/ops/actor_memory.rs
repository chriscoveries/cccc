//! `actor.memory`: what each Actor's process tree costs in RAM.
//!
//! Display only. The daemon reads `/proc` for every running Actor on the
//! activity tick's cadence and keeps the last reading in memory, so an
//! Operator can see what a harness costs on the host. Nothing else reads it:
//! delivery, idling, health, cost accounting and dispatch never look at these
//! numbers, and nothing stops or throttles an Actor for using memory.
//!
//! The reading is the Actor's *process tree* — the runtime's pid and every
//! descendant — not just the top process, because a harness (claude, codex,
//! hermes, pi) is a launcher plus its children. `rss_mib` sums `VmRSS` and
//! `pss_mib` sums `Pss` over the tree: RSS is what the host has to find, PSS is
//! what is fairly attributable to this Actor when pages are shared. Both are
//! reported because a single number hides that difference.
//!
//! A reading is either real or absent. An Actor whose runtime reports no pid —
//! a structured or browser runtime, a PTY Actor whose session is unknown — has
//! no figure, and a sample that fails (the process exited mid-walk, `/proc` is
//! unreadable) drops the reading rather than showing a stale or invented one.
//! Rows carry `null`, never `0`.

use cccc_core::GroupDoc;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;
use std::sync::{LazyLock, Mutex, PoisonError};

/// Default seconds between readings; `0` turns sampling off for the Group.
pub(crate) const DEFAULT_SAMPLE_SECONDS: u64 = 30;

const MIB: u64 = 1024;

/// One reading of a process tree, in MiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TreeReading {
    pub rss_mib: u64,
    pub pss_mib: Option<u64>,
    pub n_proc: u32,
}

/// The last reading of one Actor, with when it was taken.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sample {
    rss_mib: u64,
    pss_mib: Option<u64>,
    n_proc: u32,
    sampled_at: String,
    sampled_epoch: i64,
}

type Key = (String, String);

static SAMPLES: LazyLock<Mutex<BTreeMap<Key, Sample>>> = LazyLock::new(Mutex::default);

fn samples() -> std::sync::MutexGuard<'static, BTreeMap<Key, Sample>> {
    SAMPLES.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The Group's sampling cadence in seconds. Unset or unreadable falls back to
/// the default; `0` disables sampling.
pub(crate) fn cadence_seconds(group: &GroupDoc) -> u64 {
    group
        .extra
        .get("memory")
        .and_then(Value::as_object)
        .and_then(|memory| memory.get("sample_seconds"))
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_SAMPLE_SECONDS)
}

/// One sampling pass for one Group, from the activity publisher. Actors whose
/// reading is not due keep the one they have; Actors that are not running, or
/// that the runtime cannot locate with a pid, hold no reading at all.
pub(crate) fn sample_group(group: &GroupDoc) {
    let cadence = cadence_seconds(group);
    if cadence == 0 {
        forget(&group.group_id);
        return;
    }
    let epoch = chrono::Utc::now().timestamp();
    for actor in &group.actors {
        let key = (group.group_id.clone(), actor.id.clone());
        let status = super::actor_runtime_status::resolve(group, actor);
        let Some(pid) = status.pid.filter(|_| status.running) else {
            samples().remove(&key);
            continue;
        };
        let due = samples()
            .get(&key)
            .is_none_or(|sample| is_due(sample.sampled_epoch, epoch, cadence));
        if !due {
            continue;
        }
        match sample_tree(Path::new("/proc"), pid) {
            Ok(reading) => {
                samples().insert(
                    key,
                    Sample {
                        rss_mib: reading.rss_mib,
                        pss_mib: reading.pss_mib,
                        n_proc: reading.n_proc,
                        sampled_at: cccc_contracts::utc_now(),
                        sampled_epoch: epoch,
                    },
                );
            }
            // A failed reading is no reading: the row shows nothing rather
            // than a figure that no longer describes a live process.
            Err(_) => {
                samples().remove(&key);
            }
        }
    }
}

/// The memory fields of one `actor_list` row, or all `null` when the Actor has
/// no current reading.
pub(crate) fn row_fields(group_id: &str, actor_id: &str) -> Map<String, Value> {
    let sample = samples()
        .get(&(group_id.to_owned(), actor_id.to_owned()))
        .cloned();
    let mut fields = Map::new();
    fields.insert("rss_mib".into(), number(sample.as_ref().map(|s| s.rss_mib)));
    fields.insert(
        "pss_mib".into(),
        number(sample.as_ref().and_then(|sample| sample.pss_mib)),
    );
    fields.insert(
        "n_proc".into(),
        number(sample.as_ref().map(|sample| u64::from(sample.n_proc))),
    );
    fields.insert(
        "mem_sampled_at".into(),
        sample.map_or(Value::Null, |sample| Value::from(sample.sampled_at)),
    );
    fields
}

/// Drop every reading of one Group.
pub(crate) fn forget(group_id: &str) {
    samples().retain(|(group, _), _| group != group_id);
}

/// Drop readings of Groups that no longer exist.
pub(crate) fn retain_groups(known: &BTreeSet<String>) {
    samples().retain(|(group, _), _| known.contains(group));
}

/// Whether `now` is far enough past `last` for a fresh reading.
fn is_due(last: i64, now: i64, cadence: u64) -> bool {
    now < last || now.saturating_sub(last) >= cadence as i64
}

/// Read one process tree under `root` (a `/proc`-shaped directory). Fails when
/// the root process does not exist. Members that exit during the walk are left
/// out; an unreadable `smaps_rollup` makes `pss_mib` absent rather than partial.
pub(crate) fn sample_tree(root: &Path, pid: u32) -> io::Result<TreeReading> {
    let members = tree_members(root, pid)?;
    let root_dir = root.join(pid.to_string());
    let root_status = std::fs::read_to_string(root_dir.join("status"))?;
    let mut rss_kib = parse_kib(&root_status, "VmRSS:").unwrap_or(0);
    let mut pss_kib = read_pss(&root_dir);
    let mut counted = 1u32;
    for member in members.iter().filter(|member| **member != pid) {
        let dir = root.join(member.to_string());
        let Ok(status) = std::fs::read_to_string(dir.join("status")) else {
            continue;
        };
        rss_kib += parse_kib(&status, "VmRSS:").unwrap_or(0);
        pss_kib = match (pss_kib, read_pss(&dir)) {
            (Some(sum), Some(next)) => Some(sum + next),
            _ => None,
        };
        counted += 1;
    }
    Ok(TreeReading {
        rss_mib: rss_kib / MIB,
        pss_mib: pss_kib.map(|kib| kib / MIB),
        n_proc: counted,
    })
}

/// `pid` and every descendant, root first. A pid is a member only when its
/// parent chain reaches `pid`, so unrelated processes in the same `/proc` are
/// never summed into another Actor's figure.
fn tree_members(root: &Path, pid: u32) -> io::Result<Vec<u32>> {
    let mut parents = BTreeMap::new();
    for entry in std::fs::read_dir(root)? {
        let Ok(entry) = entry else {
            continue;
        };
        let Some(child) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(root.join(child.to_string()).join("stat")) else {
            continue;
        };
        if let Some(parent) = parse_ppid(&stat) {
            parents.insert(child, parent);
        }
    }
    if !parents.contains_key(&pid) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no process {pid}"),
        ));
    }
    let mut members = vec![pid];
    let mut queue = vec![pid];
    while let Some(parent) = queue.pop() {
        for (&child, &owner) in &parents {
            if owner == parent && child != parent && !members.contains(&child) {
                members.push(child);
                queue.push(child);
            }
        }
    }
    Ok(members)
}

fn read_pss(dir: &Path) -> Option<u64> {
    std::fs::read_to_string(dir.join("smaps_rollup"))
        .ok()
        .and_then(|text| parse_kib(&text, "Pss:"))
}

/// The parent pid from a `/proc/<pid>/stat` line: field 4, after the
/// parenthesised command name (which may itself contain spaces or brackets).
fn parse_ppid(stat: &str) -> Option<u32> {
    let close = stat.rfind(')')?;
    stat.get(close + 1..)?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// The first `key` value in kB from a `/proc` file whose lines read
/// `Key:\t<number> kB`. `Pss:` never matches `Pss_Anon:` and friends.
fn parse_kib(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        line.strip_prefix(key)?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    })
}

fn number(value: Option<u64>) -> Value {
    value.map_or(Value::Null, Value::from)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use cccc_contracts::{Actor, RunnerKind};
    use cccc_core::{GroupStore, HomeLayout, actors};

    /// A real captured tree: a codex harness on this host (root pid 2793514)
    /// with its four descendants, plus one real unrelated process (517722) in
    /// the same directory, captured 2026-10-22. See the fixture README.
    const FIXTURE_ROOT: &str = "2793514";

    fn fixture() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/actor-memory")
    }

    #[test]
    fn sums_a_real_captured_process_tree() {
        let reading = sample_tree(&fixture(), FIXTURE_ROOT.parse().expect("root pid"))
            .expect("captured tree");
        assert_eq!(reading.n_proc, 5, "the tree has five processes");
        assert_eq!(reading.rss_mib, 336, "344896 kB of VmRSS over the tree");
        assert_eq!(reading.pss_mib, Some(136), "139715 kB of Pss over the tree");
    }

    #[test]
    fn leaves_unrelated_processes_out_of_the_total() {
        // The fixture holds six real processes: the harness tree of five, plus
        // one unrelated process of 123 MiB in the same directory.
        let unrelated: u32 = 517722;
        let alone = sample_tree(&fixture(), unrelated).expect("unrelated process");
        assert_eq!(alone.n_proc, 1, "an unrelated process has no children here");
        assert_eq!(alone.rss_mib, 123, "its own real reading");
        let tree = sample_tree(&fixture(), FIXTURE_ROOT.parse().expect("root pid"))
            .expect("captured tree");
        assert_eq!(tree.n_proc, 5);
        assert_eq!(
            tree.rss_mib, 336,
            "459 MiB would mean the unrelated process was summed in"
        );
    }

    #[test]
    fn missing_pss_is_absent_not_partial() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("proc");
        copy_tree(&fixture(), &root);
        std::fs::remove_file(root.join("2793848/smaps_rollup")).expect("remove one reading");
        let reading = sample_tree(&root, FIXTURE_ROOT.parse().expect("root pid")).expect("tree");
        assert_eq!(reading.rss_mib, 336, "RSS still sums");
        assert_eq!(reading.n_proc, 5);
        assert_eq!(reading.pss_mib, None, "a partial Pss is not reported");
    }

    #[test]
    fn vanished_root_is_an_error() {
        let error = sample_tree(&fixture(), 999_999).expect_err("no such process");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn cadence_is_due_after_the_interval() {
        assert!(!is_due(100, 129, 30), "still inside the interval");
        assert!(is_due(100, 130, 30), "exactly the interval");
        assert!(is_due(100, 99, 30), "a clock that went backwards resamples");
    }

    #[test]
    fn row_fields_are_null_without_a_reading() {
        let fields = row_fields("no-such-group", "no-such-actor");
        for key in ["rss_mib", "pss_mib", "n_proc", "mem_sampled_at"] {
            assert_eq!(fields[key], Value::Null, "{key} must be null, never zero");
        }
    }

    #[test]
    fn samples_a_real_process_tree() {
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 5 & sleep 5 & wait")
            .spawn()
            .expect("spawn a real tree");
        let pid = child.id();
        let mut members = Vec::new();
        let mut reading = None;
        for _ in 0..60 {
            if let Ok(current) = sample_tree(Path::new("/proc"), pid)
                && current.n_proc >= 3
            {
                members = tree_members(Path::new("/proc"), pid).unwrap_or_default();
                reading = Some(current);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // Take the whole tree down, children included: a test must leave no
        // process of its own behind.
        for member in members {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(member as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let _ = child.kill();
        let _ = child.wait();
        let reading = reading.expect("the spawned tree never showed three processes");
        assert!(reading.rss_mib > 0, "a live tree has resident memory");
        assert!(
            reading.pss_mib.is_some_and(|pss| pss > 0),
            "a live tree has attributable memory"
        );
        assert!(reading.pss_mib.expect("pss") <= reading.rss_mib + 1);
    }

    #[test]
    fn a_process_that_has_exited_has_no_reading() {
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .expect("spawn");
        let pid = child.id();
        child.wait().expect("reap");
        let error = sample_tree(Path::new("/proc"), pid).expect_err("the process is gone");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn samples_a_running_actor_and_drops_it_when_it_stops() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home).expect("store");
        let group = store.create("memory", "").expect("group");
        store
            .mutate(&group.group_id, |doc| {
                let mut actor = Actor::new("peer");
                actor.runner = RunnerKind::Pty;
                actors::add(doc, actor)
            })
            .expect("actor");
        cccc_runtime::start(cccc_runtime::LaunchSpec {
            group_id: group.group_id.clone(),
            actor_id: "peer".into(),
            runner: RunnerKind::Pty,
            command: vec!["sh".into(), "-c".into(), "sleep 10".into()],
            cwd: temp.path().into(),
            env: BTreeMap::new(),
            cols: 80,
            rows: 24,
        })
        .expect("runtime");

        let group = store.load(&group.group_id).expect("reload");
        sample_group(&group);
        let fields = row_fields(&group.group_id, "peer");
        assert!(
            fields["rss_mib"].as_u64().is_some_and(|mib| mib > 0),
            "a running Actor shows its tree: {fields:?}"
        );
        assert!(fields["n_proc"].as_u64().is_some_and(|procs| procs >= 1));
        assert!(fields["mem_sampled_at"].is_string());

        cccc_runtime::stop(&group.group_id, "peer").expect("stop");
        let group = store.load(&group.group_id).expect("reload");
        sample_group(&group);
        let fields = row_fields(&group.group_id, "peer");
        for key in ["rss_mib", "pss_mib", "n_proc", "mem_sampled_at"] {
            assert_eq!(
                fields[key],
                Value::Null,
                "a stopped Actor holds no reading: {key}"
            );
        }
    }

    #[test]
    fn zero_cadence_disables_sampling() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let store = GroupStore::new(home).expect("store");
        let group = store.create("memory off", "").expect("group");
        store
            .mutate(&group.group_id, |doc| {
                doc.extra
                    .insert("memory".into(), serde_json::json!({"sample_seconds": 0}));
                Ok(())
            })
            .expect("settings");
        let group = store.load(&group.group_id).expect("reload");
        assert_eq!(cadence_seconds(&group), 0);
        assert!(!group.extra.is_empty());
    }

    fn copy_tree(from: &Path, to: &Path) {
        for entry in std::fs::read_dir(from).expect("fixture") {
            let entry = entry.expect("entry");
            if !entry.path().is_dir() {
                continue;
            }
            let target = to.join(entry.file_name());
            std::fs::create_dir_all(&target).expect("dir");
            for file in std::fs::read_dir(entry.path()).expect("pid dir") {
                let file = file.expect("file");
                std::fs::copy(file.path(), target.join(file.file_name())).expect("copy");
            }
        }
    }
}
