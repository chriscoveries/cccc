//! Host scan for lane processes that outlived their owner.
//!
//! Every actor runtime is launched with `CCCC_GROUP_ID` / `CCCC_ACTOR_ID` in
//! its environment. The abrupt-exit guard (see `guard`) terminates recorded
//! process *groups* when their owner dies, but a process can still escape
//! that net: it may have been reparented out of its group, spawned before the
//! guard existed, or left behind by a watchdog that was itself killed. Those
//! strays keep their `CCCC_*` environment, so a host scan finds them even
//! though no owner record points at them.
//!
//! This module only *finds* tagged processes. Deciding whether one is a
//! leftover (dead group, removed actor, stale generation, stopped actor) and
//! acting on it belongs to the daemon, which owns group and actor state.

/// One host process carrying lane identity in its environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaggedProcess {
    /// OS process id.
    pub pid: i32,
    /// Process group id at scan time (0 when unreadable).
    pub pgid: i32,
    /// Approximate start time as unix seconds (directory mtime of
    /// `/proc/<pid>`; second resolution is plenty for hour-scale age gates).
    pub started_secs: u64,
    /// Short command name (`/proc/<pid>/comm`), best effort.
    pub comm: String,
    /// Value of `CCCC_ACTOR_ID` (non-empty by construction).
    pub actor_id: String,
    /// Value of `CCCC_GROUP_ID` (non-empty by construction).
    pub group_id: String,
}

/// Scan the host for processes tagged with lane identity.
///
/// Linux reads `/proc/<pid>/environ` directly. Other platforms return an
/// empty vec: macOS would need `KERN_PROCARGS2` per pid and Windows has no
/// portable peer-environ read, and an unverifiable implementation here would
/// be worse than an explicit gap.
pub fn scan_tagged_processes() -> Vec<TaggedProcess> {
    #[cfg(target_os = "linux")]
    {
        scan_proc()
    }
    #[cfg(target_os = "macos")]
    {
        super::macos::scan_macos()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        // Windows has no portable peer-environ read; an unverifiable
        // implementation would be worse than this explicit gap.
        Vec::new()
    }
}

/// Parse a NUL-separated `/proc/<pid>/environ` block. Returns the
/// `(actor_id, group_id)` pair when both are present and non-blank after
/// trimming; anything else is not attributable and stays invisible.
///
/// Linux-only in production (the macOS path parses `ps` output instead), but
/// compiled under test everywhere so the contract stays checked.
#[cfg(any(test, target_os = "linux"))]
pub fn parse_environ_tags(block: &[u8]) -> Option<(String, String)> {
    let mut actor: Option<&str> = None;
    let mut group: Option<&str> = None;
    for entry in block.split(|byte| *byte == 0) {
        let entry = std::str::from_utf8(entry).unwrap_or_default();
        let Some((key, value)) = entry.split_once('=') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key {
            "CCCC_ACTOR_ID" => actor = Some(value),
            "CCCC_GROUP_ID" => group = Some(value),
            _ => {}
        }
    }
    match (actor, group) {
        (Some(actor), Some(group)) => Some((actor.to_owned(), group.to_owned())),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn scan_proc() -> Vec<TaggedProcess> {
    let own_pid = std::process::id() as i32;
    let mut tagged = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return tagged;
    };
    for entry in dir.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(pid) = name.parse::<i32>() else {
            continue;
        };
        if pid <= 0 || pid == own_pid {
            continue;
        }
        let base = format!("/proc/{pid}");
        let Ok(environ) = std::fs::read(format!("{base}/environ")) else {
            continue;
        };
        let Some((actor_id, group_id)) = parse_environ_tags(&environ) else {
            continue;
        };
        let started_secs = std::fs::metadata(&base)
            .and_then(|meta| meta.modified())
            .map(|time| {
                time.duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_secs())
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        let comm = std::fs::read_to_string(format!("{base}/comm"))
            .map(|text| text.trim().to_owned())
            .unwrap_or_default();
        // `stat` is read-only and portable; failure just loses the pgid.
        // Fields after the last ')' are: state, ppid, pgrp, ... — pgrp is
        // the THIRD token (an earlier revision read ppid here and labelled
        // it pgid; the own-tree exclusion below depends on the real pgrp).
        let pgid = std::fs::read_to_string(format!("{base}/stat"))
            .ok()
            .and_then(|stat| {
                // comm may contain spaces/parens, so split after the last ')'.
                let after = stat.rsplit(')').next()?;
                let mut fields = after.split_whitespace();
                fields.next()?; // state
                fields.next()?; // ppid
                fields.next().and_then(|field| field.parse::<i32>().ok()) // pgrp
            })
            .unwrap_or(0);
        tagged.push(TaggedProcess {
            pid,
            pgid,
            started_secs,
            comm,
            actor_id,
            group_id,
        });
    }
    tagged.sort_by_key(|process| process.pid);
    tagged
}

/// Terminate a leftover and the tree under it, escalating from TERM to KILL.
/// Returns true when the pid is gone afterwards. Best effort throughout: a
/// process that exits between the checks is a success, not an error.
///
/// - `expected_started_secs` re-verifies identity immediately before the
///   first signal: a recycled pid (same number, newer start) aborts, `false`.
///   Zero means "unknown" (a fallback scan hit) and skips the check — the age
///   gate already keeps such processes out of the reaper.
/// - When the pid leads its own sane process group (`pid == pgid > 1`), the
///   whole group is signalled, the way the abrupt-exit guard does: killing
///   one shell must not leave its children behind. Otherwise the pid plus its
///   currently-visible descendants are signalled.
pub fn terminate_process(
    pid: i32,
    pgid: i32,
    expected_started_secs: u64,
    grace: std::time::Duration,
) -> bool {
    #[cfg(unix)]
    {
        if pid <= 1 {
            return false;
        }
        if expected_started_secs > 0
            && current_started_secs(pid) != Some(expected_started_secs)
        {
            // Recycled pid (or exit race): not ours to signal.
            return false;
        }
        // Send TERM through kill(1): nix is not a dependency of this crate,
        // and a shell-free direct exec keeps this auditable.
        if pgid > 1 && pid == pgid {
            let _ = std::process::Command::new("kill")
                .args(["-s", "TERM", &format!("-{pgid}")])
                .status();
        } else {
            for victim in tree_pids(pid) {
                let _ = std::process::Command::new("kill")
                    .args(["-s", "TERM", &victim.to_string()])
                    .status();
            }
        }
        let deadline = std::time::Instant::now() + grace;
        loop {
            if !pid_alive(pid) {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        if pgid > 1 && pid == pgid {
            let _ = std::process::Command::new("kill")
                .args(["-s", "KILL", &format!("-{pgid}")])
                .status();
        } else {
            for victim in tree_pids(pid) {
                let _ = std::process::Command::new("kill")
                    .args(["-s", "KILL", &victim.to_string()])
                    .status();
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        !pid_alive(pid)
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, pgid, expected_started_secs, grace);
        false
    }
}

/// Current start time of a pid in unix seconds, same source the scan uses.
/// `None` when the pid is gone or unreadable.
#[cfg(target_os = "linux")]
fn current_started_secs(pid: i32) -> Option<u64> {
    let base = format!("/proc/{pid}");
    std::fs::metadata(&base)
        .and_then(|meta| meta.modified())
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .ok()
        .filter(|secs| *secs > 0)
}

/// Same contract via a single-pid `ps` etime lookup.
#[cfg(target_os = "macos")]
fn current_started_secs(pid: i32) -> Option<u64> {
    let output = std::process::Command::new("ps")
        .args(["-o", "etime=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    let elapsed = super::macos::parse_etime(&String::from_utf8_lossy(&output.stdout))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let started = now.checked_sub(elapsed)?;
    (started > 0).then_some(started)
}

/// Pids of `pid` plus its currently-visible descendants (children,
/// grandchildren), oldest-first. Best effort: an exit race just shrinks the
/// set, and anything spawned later is caught by the pre-KILL re-snapshot.
#[cfg(target_os = "linux")]
fn tree_pids(pid: i32) -> Vec<i32> {
    let mut children: std::collections::HashMap<i32, Vec<i32>> = std::collections::HashMap::new();
    let mut all = vec![pid];
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return all;
    };
    for entry in dir.flatten() {
        let name = entry.file_name();
        let Ok(candidate) = name.to_string_lossy().parse::<i32>() else {
            continue;
        };
        if candidate <= 0 {
            continue;
        }
        let ppid = std::fs::read_to_string(format!("/proc/{candidate}/stat"))
            .ok()
            .and_then(|stat| {
                let after = stat.rsplit(')').next()?;
                let mut fields = after.split_whitespace();
                fields.next()?; // state
                fields.next()?.parse::<i32>().ok() // ppid
            });
        if let Some(ppid) = ppid {
            children.entry(ppid).or_default().push(candidate);
        }
    }
    // Breadth-first from the root pid; the root itself stays first so a
    // group-leader check elsewhere keeps working on ordered output.
    let mut queue = vec![pid];
    while let Some(parent) = queue.pop() {
        if let Some(kids) = children.remove(&parent) {
            for kid in kids {
                if !all.contains(&kid) {
                    all.push(kid);
                    queue.push(kid);
                }
            }
        }
    }
    all
}

/// Same contract via one `ps -A -o pid=,ppid=` pass.
#[cfg(target_os = "macos")]
fn tree_pids(pid: i32) -> Vec<i32> {
    let mut children: std::collections::HashMap<i32, Vec<i32>> = std::collections::HashMap::new();
    let mut all = vec![pid];
    let Ok(output) = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid="])
        .env("LC_ALL", "C")
        .output()
    else {
        return all;
    };
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut tokens = line.split_whitespace();
        let (Some(candidate), Some(ppid)) = (tokens.next(), tokens.next()) else {
            continue;
        };
        let (Ok(candidate), Ok(ppid)) = (candidate.parse::<i32>(), ppid.parse::<i32>()) else {
            continue;
        };
        if candidate <= 0 || ppid < 0 {
            continue;
        }
        children.entry(ppid).or_default().push(candidate);
    }
    let mut queue = vec![pid];
    while let Some(parent) = queue.pop() {
        if let Some(kids) = children.remove(&parent) {
            for kid in kids {
                if !all.contains(&kid) {
                    all.push(kid);
                    queue.push(kid);
                }
            }
        }
    }
    all
}

/// True while `/proc/<pid>` exists and is not a zombie. A zombie still proves
/// identity but cannot be signalled; for reaping purposes it counts as gone
/// (its parent, not us, must reap it). Same rule as the abrupt-exit guard,
/// which treats zombie-only groups as ended (`guard.rs`).
#[cfg(target_os = "linux")]
fn pid_alive(pid: i32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some(after) = stat.rsplit(')').next() else {
        return false;
    };
    match after.split_whitespace().next() {
        // 'Z' is zombie: gone for our purposes.
        Some(state) => state != "Z",
        None => false,
    }
}

/// Same contract on macOS via `ps -o state=`: a zombie ('Z') counts as gone,
/// mirroring the guard, which macOS refuses to signal. Anything unparseable
/// counts as gone too — the pre-TERM identity recheck already established
/// this pid was ours, so a vanishing pid is success, not a reason to KILL an
/// unknown successor.
#[cfg(target_os = "macos")]
fn pid_alive(pid: i32) -> bool {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-o", "state=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .output()
    else {
        return false;
    };
    match String::from_utf8_lossy(&output.stdout).trim() {
        "" => false,
        state => !state.starts_with('Z'),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environ_tags_need_both_ids_non_blank() {
        let block = b"PATH=/bin\0CCCC_ACTOR_ID=peer-a\0CCCC_GROUP_ID=g1\0";
        assert_eq!(
            parse_environ_tags(block),
            Some(("peer-a".into(), "g1".into()))
        );
        // Missing actor id: not attributable, stays invisible.
        assert_eq!(parse_environ_tags(b"CCCC_GROUP_ID=g1\0"), None);
        // Missing group id: same.
        assert_eq!(parse_environ_tags(b"CCCC_ACTOR_ID=peer-a\0"), None);
        // Blank values do not count.
        assert_eq!(
            parse_environ_tags(b"CCCC_ACTOR_ID=  \0CCCC_GROUP_ID=g1\0"),
            None
        );
        // Values are trimmed.
        assert_eq!(
            parse_environ_tags(b"CCCC_ACTOR_ID= peer-a \0CCCC_GROUP_ID= g1 \0"),
            Some(("peer-a".into(), "g1".into()))
        );
        // Non-UTF8 entries are skipped, valid tags still parse.
        let mut block = b"CCCC_ACTOR_ID=peer-a\0 odd=\xff\xfe \0CCCC_GROUP_ID=g1\0".to_vec();
        assert_eq!(
            parse_environ_tags(&block),
            Some(("peer-a".into(), "g1".into()))
        );
        block.clear();
        assert_eq!(parse_environ_tags(&block), None);
        assert_eq!(parse_environ_tags(b"\0\0"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn scan_finds_a_tagged_child_and_skips_self() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .env("CCCC_ACTOR_ID", "leftover-test-actor")
            .env("CCCC_GROUP_ID", "leftover-test-group")
            .spawn()
            .expect("spawn tagged sleep");
        // Give /proc a moment; the scan itself is instantaneous.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let found = scan_tagged_processes()
            .into_iter()
            .find(|process| process.pid == child.id() as i32);
        let found = found.expect("tagged child is found");
        assert_eq!(found.actor_id, "leftover-test-actor");
        assert_eq!(found.group_id, "leftover-test-group");
        assert_eq!(found.comm, "sleep");
        assert!(found.started_secs > 0);
        assert_ne!(found.pid, std::process::id() as i32);
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn terminate_process_kills_a_tagged_sleep() {
        let mut child = std::process::Command::new("sleep")
            .arg("300")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as i32;
        let started = current_started_secs(pid).expect("start time");
        assert!(terminate_process(pid, 0, started, std::time::Duration::from_secs(2)));
        let _ = child.wait();
        assert!(!pid_alive(pid));
    }

    /// C2: a group leader's whole tree goes, not just the leader. A shell
    /// that spawned a child in its own group must not leave the child behind.
    #[cfg(target_os = "linux")]
    #[test]
    fn terminate_group_leader_clears_its_children() {
        use std::os::unix::process::CommandExt as _;
        let mut leader = std::process::Command::new("sh")
            .args(["-c", "sleep 300 & wait"])
            .process_group(0)
            .spawn()
            .expect("spawn leader");
        let pid = leader.id() as i32;
        std::thread::sleep(std::time::Duration::from_millis(500));
        let tree = tree_pids(pid);
        // The shell plus its sleep child: more than the leader alone.
        assert!(tree.len() >= 2, "expected a child, got {tree:?}");
        let started = current_started_secs(pid).expect("start time");
        assert!(terminate_process(pid, pid, started, std::time::Duration::from_secs(5)));
        let _ = leader.wait();
        for member in tree {
            assert!(!pid_alive(member), "straggler survived: {member}");
        }
    }

    /// C3: a recycled pid (same number, newer start) is never signalled.
    #[cfg(target_os = "linux")]
    #[test]
    fn terminate_process_aborts_on_start_time_mismatch() {
        let mut child = std::process::Command::new("sleep")
            .arg("300")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as i32;
        // Claim it started at epoch: nothing alive matches that fingerprint.
        assert!(!terminate_process(pid, 0, 1, std::time::Duration::from_secs(2)));
        // And the process is untouched.
        assert!(pid_alive(pid));
        let _ = child.kill();
        let _ = child.wait();
    }

    /// The scan reports the real process group, not the parent pid: the
    /// own-tree exclusion depends on it.
    #[cfg(target_os = "linux")]
    #[test]
    fn scan_reports_pgrp_not_ppid() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .env("CCCC_ACTOR_ID", "pgid-check")
            .env("CCCC_GROUP_ID", "pgid-check-group")
            .spawn()
            .expect("spawn sleep");
        std::thread::sleep(std::time::Duration::from_millis(200));
        let found = scan_tagged_processes()
            .into_iter()
            .find(|process| process.pid == child.id() as i32)
            .expect("tagged child is found");
        // A plain spawn shares our process group: pgid == our pgid, while
        // ppid would be our pid. They must differ here (we are not pid 1's
        // group leader in the test harness... assert the weaker invariant:
        // pgid reads as OUR pgid, not our pid).
        let own_pgid = std::fs::read_to_string("/proc/self/stat")
            .ok()
            .and_then(|stat| {
                let after = stat.rsplit(')').next()?;
                let mut fields = after.split_whitespace();
                fields.next()?;
                fields.next()?;
                fields.next().and_then(|field| field.parse::<i32>().ok())
            })
            .unwrap_or(0);
        assert!(own_pgid > 0);
        assert_eq!(found.pgid, own_pgid);
        let _ = child.kill();
        let _ = child.wait();
    }
}
