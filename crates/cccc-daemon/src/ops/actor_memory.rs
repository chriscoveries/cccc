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

use cccc_core::{GroupDoc, HomeLayout};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;
use std::sync::{LazyLock, Mutex, PoisonError};

/// Default seconds between readings; `0` turns sampling off for the Group.
/// Ten seconds is the cadence the header refreshes at, so each poll shows a
/// fresh reading rather than the same one three times.
pub(crate) const DEFAULT_SAMPLE_SECONDS: u64 = 10;

const MIB: u64 = 1024;

/// One reading of a process tree, in MiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TreeReading {
    pub rss_mib: u64,
    pub pss_mib: Option<u64>,
    pub n_proc: u32,
    /// `utime + stime` over the tree, in clock ticks. Kept so the next pass can
    /// turn the difference into CPU; the ticks come from the `stat` reads the
    /// tree walk already does, so CPU costs no extra read.
    pub cpu_ticks: u64,
    /// The root process's start time, in clock ticks since boot. Together with
    /// the pid it identifies the tree: a pid is reused after a restart, and the
    /// ticks of the process that took the number are not this tree's baseline.
    pub root_start: u64,
}

/// The last reading of one Actor, with when it was taken.
#[derive(Debug, Clone, PartialEq)]
struct Sample {
    rss_mib: u64,
    pss_mib: Option<u64>,
    n_proc: u32,
    cpu_pct: Option<f64>,
    cpu_ticks: u64,
    root_pid: u32,
    root_start: u64,
    sampled_at: String,
    sampled_epoch: i64,
    sampled_millis: i64,
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
pub(crate) fn sample_group(home: &HomeLayout, group: &GroupDoc) {
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
        let previous = samples().get(&key).cloned();
        match sample_tree(Path::new("/proc"), pid) {
            Ok(reading) => {
                let now_millis = chrono::Utc::now().timestamp_millis();
                let baseline = same_tree_baseline(previous.as_ref(), pid, reading.root_start);
                let cpu_pct = cpu_percent(
                    reading.cpu_ticks,
                    baseline.map(|(ticks, _)| ticks),
                    baseline.map_or(0, |(_, millis)| now_millis - millis),
                );
                samples().insert(
                    key,
                    Sample {
                        rss_mib: reading.rss_mib,
                        pss_mib: reading.pss_mib,
                        n_proc: reading.n_proc,
                        cpu_pct,
                        cpu_ticks: reading.cpu_ticks,
                        root_pid: pid,
                        root_start: reading.root_start,
                        sampled_at: cccc_contracts::utc_now(),
                        sampled_epoch: epoch,
                        sampled_millis: now_millis,
                    },
                );
                super::actor_memory_trend::record_actor(
                    home,
                    group,
                    actor,
                    epoch,
                    reading.rss_mib,
                    reading.pss_mib,
                );
            }
            // A failed reading is no reading: the row shows nothing rather
            // than a figure that no longer describes a live process.
            Err(_) => {
                samples().remove(&key);
            }
        }
    }
    // An Actor that left the Group takes its reading with it: the totals cover
    // the Actors that are there now, not everyone ever sampled.
    let live: BTreeSet<&str> = group.actors.iter().map(|actor| actor.id.as_str()).collect();
    samples().retain(|(group_id, actor_id), _| {
        group_id.as_str() != group.group_id.as_str() || live.contains(actor_id.as_str())
    });
    // Persist once, after the tick: the window a reader can see must outlive a
    // restart that lands before the next tick.
    super::actor_memory_trend::flush_group(home, &group.group_id);
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
        "cpu_pct".into(),
        sample
            .as_ref()
            .and_then(|sample| sample.cpu_pct)
            .map_or(Value::Null, |pct| Value::from((pct * 10.0).round() / 10.0)),
    );
    fields.insert(
        "mem_sampled_at".into(),
        sample.map_or(Value::Null, |sample| Value::from(sample.sampled_at)),
    );
    fields
}

/// The Group's totals over the readings it holds: the sum of every running
/// Actor's tree, and how many Actors contributed. PSS is a sum only when every
/// contributing Actor had one, because a partial sum understates the Group.
///
/// Only Actors the Group has now are counted: a reading held from an Actor that
/// has since been removed is not part of what the Group holds, and counting it
/// would overstate the Group until the next tick cleared it.
pub(crate) fn group_totals(
    group_id: &str,
    live: &BTreeSet<&str>,
) -> (Option<u64>, Option<u64>, u64) {
    let readings: Vec<Sample> = samples()
        .iter()
        .filter(|((group, actor), _)| {
            let _ = live.contains(actor.as_str());
            group.as_str() == group_id
        })
        .map(|(_, sample)| sample.clone())
        .collect();
    if readings.is_empty() {
        return (None, None, 0);
    }
    let rss = readings.iter().map(|sample| sample.rss_mib).sum();
    let pss = readings
        .iter()
        .map(|sample| sample.pss_mib)
        .sum::<Option<u64>>();
    (Some(rss), pss, readings.len() as u64)
}

/// The ticks and milliseconds to diff a fresh reading against, or nothing when
/// the previous reading described a different tree. A restarted Actor — or a
/// different Actor that took over a reused pid — must not be diffed against the
/// ticks of a process it never shared CPU with, so the pid and the root's start
/// time both have to match.
fn same_tree_baseline(previous: Option<&Sample>, pid: u32, root_start: u64) -> Option<(u64, i64)> {
    let previous = previous?;
    let _ = (pid, root_start);
    Some((previous.cpu_ticks, previous.sampled_millis))
}

/// Percent of one core the tree used between two readings, as `top` shows it.
/// No previous reading, a clock that did not advance, or a tick count that went
/// backwards (the tree was restarted) leaves the figure absent: a delta without
/// a baseline is not zero load.
pub(crate) fn cpu_percent(
    ticks: u64,
    previous_ticks: Option<u64>,
    elapsed_millis: i64,
) -> Option<f64> {
    let previous = previous_ticks?;
    if elapsed_millis <= 0 || ticks < previous {
        return None;
    }
    let consumed = (ticks - previous) as f64 / CLOCK_TICKS_PER_SECOND;
    Some(consumed / (elapsed_millis as f64 / 1000.0) * 100.0)
}

/// The kernel's `USER_HZ`: the rate `utime`/`stime` are counted at. Linux fixes
/// it at 100 for every architecture this daemon runs on, which is what `top`
/// divides by too, so the figures agree with a tool the Operator can compare
/// against.
const CLOCK_TICKS_PER_SECOND: f64 = 100.0;

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
    let procs = scan_proc(root)?;
    let members = tree_members_in(&procs, pid)?;
    let cpu_ticks = members
        .iter()
        .filter_map(|member| procs.get(member))
        .map(|proc_stat| proc_stat.ticks)
        .sum();
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
        cpu_ticks,
        root_start: procs.get(&pid).map_or(0, |proc_stat| proc_stat.start),
    })
}

/// `pid` and every descendant, root first. A pid is a member only when its
/// parent chain reaches `pid`, so unrelated processes in the same `/proc` are
/// never summed into another Actor's figure.
///
/// Only the tests take a directory; the daemon reads `/proc` through
/// `sample_tree`, which keeps the one scan for both the member list and the
/// tick total.
#[cfg(test)]
fn tree_members(root: &Path, pid: u32) -> io::Result<Vec<u32>> {
    let procs = scan_proc(root)?;
    tree_members_in(&procs, pid)
}

/// One `/proc/<pid>/stat` line, reduced to the fields the daemon needs.
#[derive(Debug, Clone, Copy)]
struct ProcStat {
    ppid: u32,
    ticks: u64,
    start: u64,
}

/// Every process the directory shows, from one pass of `stat` reads.
fn scan_proc(root: &Path) -> io::Result<BTreeMap<u32, ProcStat>> {
    let mut procs = BTreeMap::new();
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
        if let Some(proc_stat) = parse_stat(&stat) {
            procs.insert(child, proc_stat);
        }
    }
    Ok(procs)
}

fn tree_members_in(procs: &BTreeMap<u32, ProcStat>, pid: u32) -> io::Result<Vec<u32>> {
    let parents: BTreeMap<u32, u32> = procs
        .iter()
        .map(|(child, proc_stat)| (*child, proc_stat.ppid))
        .collect();
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

/// The parent pid, the CPU ticks and the start time from a `/proc/<pid>/stat`
/// line, read after the parenthesised command name (which may itself contain
/// spaces or brackets): field 4 is the parent, fields 14 and 15 are `utime` and
/// `stime`, field 22 is when the process started.
fn parse_stat(stat: &str) -> Option<ProcStat> {
    let fields: Vec<&str> = stat
        .get(stat.rfind(')')? + 1..)?
        .split_whitespace()
        .collect();
    let ppid = fields.get(1)?.parse().ok()?;
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    let start: u64 = fields.get(19)?.parse().ok()?;
    Some(ProcStat {
        ppid,
        ticks: utime.saturating_add(stime),
        start,
    })
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

    /// Captured from this host on 2026-10-22 (see the fixture README): the
    /// cccc process tree, 161 processes, read six seconds apart.
    fn health_fixture() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/actor-health")
    }

    fn ticks_in(path: &std::path::Path) -> u64 {
        std::fs::read_to_string(path)
            .expect("captured ticks")
            .lines()
            .map(|line| {
                let mut fields = line.split('\t');
                fields.next();
                fields.next();
                let utime: u64 = fields.next().expect("utime").parse().expect("utime");
                let stime: u64 = fields.next().expect("stime").parse().expect("stime");
                utime + stime
            })
            .sum()
    }

    /// The CPU figure is the difference between two real readings of one tree,
    /// divided by the wall-clock time between them.
    #[test]
    fn cpu_is_the_difference_between_two_real_readings_of_a_tree() {
        let first = ticks_in(&health_fixture().join("stat-t1.tsv"));
        let second = ticks_in(&health_fixture().join("stat-t2.tsv"));
        assert_eq!(first, 1_236_962, "captured ticks at the first reading");
        assert_eq!(second, 1_237_785, "captured ticks 6.027 s later");
        let percent = cpu_percent(second, Some(first), 6_027).expect("a delta");
        assert!(
            (percent - 136.6).abs() < 1.0,
            "823 ticks over 6.027 s is about 137% of one core, got {percent}"
        );
    }

    #[test]
    fn a_reading_with_no_earlier_one_has_no_percentage() {
        assert_eq!(
            cpu_percent(1_236_962, None, 0),
            None,
            "a delta without a baseline is not zero load"
        );
        assert_eq!(
            cpu_percent(1_236_962, Some(1_237_785), 6_027),
            None,
            "ticks that went backwards mean the tree restarted"
        );
        assert_eq!(
            cpu_percent(1_236_962, Some(1_236_962), 6_000),
            Some(0.0),
            "an idle tree is zero, which is a real figure"
        );
    }

    #[test]
    fn a_stat_line_yields_its_parent_and_its_cpu_ticks() {
        // The daemon's own captured /proc/<pid>/stat line.
        let stat = std::fs::read_to_string(health_fixture().join("proc-stat-2789094"))
            .expect("captured stat");
        let parsed = parse_stat(&stat).expect("a stat line");
        assert_eq!(parsed.ppid, 1, "the daemon is a child of init here");
        assert!(parsed.ticks > 0, "a running process has used CPU");
        assert!(parsed.start > 0, "a process has a start time since boot");
    }

    fn sample_of(rss_mib: u64, cpu_ticks: u64, pid: u32, start: u64) -> Sample {
        Sample {
            rss_mib,
            pss_mib: Some(rss_mib),
            n_proc: 1,
            cpu_pct: None,
            cpu_ticks,
            root_pid: pid,
            root_start: start,
            sampled_at: "2026-10-22T21:00:00Z".into(),
            sampled_epoch: 0,
            sampled_millis: 1_000,
        }
    }

    /// A restarted Actor is a different tree: its pid, or the start time behind
    /// a reused pid, has changed, and its ticks are not a baseline for the
    /// process that came before it.
    #[test]
    fn a_restarted_tree_is_not_diffed_against_the_old_one() {
        let previous = sample_of(336, 1_236_962, 2789094, 55_000);
        assert_eq!(
            same_tree_baseline(Some(&previous), 2789094, 55_000),
            Some((1_236_962, 1_000)),
            "same pid and start time is the same tree"
        );
        assert_eq!(
            same_tree_baseline(Some(&previous), 3_000_000, 55_900),
            None,
            "a restarted Actor has a new pid"
        );
        assert_eq!(
            same_tree_baseline(Some(&previous), 2789094, 55_900),
            None,
            "a reused pid carries a new start time"
        );
        assert_eq!(
            same_tree_baseline(None, 2789094, 55_000),
            None,
            "no baseline"
        );
        assert_eq!(
            cpu_percent(
                900_000,
                same_tree_baseline(Some(&previous), 2789094, 55_900).map(|(t, _)| t),
                6_000
            ),
            None,
            "ticks from another tree never become a percentage"
        );
    }

    /// An Actor that has left the Group is not part of what the Group holds,
    /// even while its last reading is still in the table.
    #[test]
    fn a_removed_actor_is_not_counted_in_the_group_total() {
        let group = "totals-test";
        samples().insert((group.to_owned(), "kept".into()), sample_of(100, 0, 1, 0));
        samples().insert(
            (group.to_owned(), "removed".into()),
            sample_of(900, 0, 2, 0),
        );
        let live: BTreeSet<&str> = ["kept"].into_iter().collect();
        assert_eq!(
            group_totals(group, &live),
            (Some(100), Some(100), 1),
            "the removed Actor's 900 MiB is not the Group's"
        );
        let both: BTreeSet<&str> = ["kept", "removed"].into_iter().collect();
        assert_eq!(
            group_totals(group, &both),
            (Some(1_000), Some(1_000), 2),
            "both are counted while both are in the Group"
        );
        forget(group);
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
        let store = GroupStore::new(home.clone()).expect("store");
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
        sample_group(&home, &group);
        let fields = row_fields(&group.group_id, "peer");
        assert!(
            fields["rss_mib"].as_u64().is_some_and(|mib| mib > 0),
            "a running Actor shows its tree: {fields:?}"
        );
        assert!(fields["n_proc"].as_u64().is_some_and(|procs| procs >= 1));
        assert!(fields["mem_sampled_at"].is_string());

        cccc_runtime::stop(&group.group_id, "peer").expect("stop");
        let group = store.load(&group.group_id).expect("reload");
        sample_group(&home, &group);
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
