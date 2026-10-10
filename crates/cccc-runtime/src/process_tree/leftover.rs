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
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}

/// Parse a NUL-separated `/proc/<pid>/environ` block. Returns the
/// `(actor_id, group_id)` pair when both are present and non-blank after
/// trimming; anything else is not attributable and stays invisible.
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
        let pgid = std::fs::read_to_string(format!("{base}/stat"))
            .ok()
            .and_then(|stat| {
                // comm may contain spaces/parens, so split after the last ')'.
                let after = stat.rsplit(')').next()?;
                let mut fields = after.split_whitespace();
                fields.next()?; // state
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

/// Terminate one process, escalating from TERM to KILL. Returns true when the
/// pid is gone afterwards. Best effort throughout: a process that exits
/// between the checks is a success, not an error.
pub fn terminate_process(pid: i32, grace: std::time::Duration) -> bool {
    #[cfg(unix)]
    {
        // Send TERM through kill(1): nix is not a dependency of this crate,
        // and a shell-free direct exec keeps this auditable.
        let _ = std::process::Command::new("kill")
            .args(["-s", "TERM", &pid.to_string()])
            .status();
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
        let _ = std::process::Command::new("kill")
            .args(["-s", "KILL", &pid.to_string()])
            .status();
        std::thread::sleep(std::time::Duration::from_millis(500));
        !pid_alive(pid)
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, grace);
        false
    }
}

/// True while `/proc/<pid>` exists and is not a zombie. A zombie still proves
/// identity but cannot be signalled; for reaping purposes it counts as gone
/// (its parent, not us, must reap it).
#[cfg(unix)]
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
        assert!(terminate_process(pid, std::time::Duration::from_secs(2)));
        let _ = child.wait();
        assert!(!pid_alive(pid));
    }
}
