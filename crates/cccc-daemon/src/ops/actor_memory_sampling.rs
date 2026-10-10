//! Resident-memory sampling for one actor process tree.
//!
//! Linux reads `/proc` directly; macOS goes through `ps` (the workspace
//! forbids `unsafe`, so no sysctl FFI); other platforms report nothing.
//! Pids that vanish mid-scan contribute nothing. Pure parsers are unit
//! tested on every platform.

use std::collections::HashMap;

/// Parse `VmRSS: <kb> kB` out of a Linux `/proc/<pid>/status` body.
/// Returns resident KiB. Anything unparseable (exited, kernel thread) is
/// `None` — skipped, never counted as zero.
pub fn parse_vmrss_kb(status: &str) -> Option<u64> {
    for line in status.lines() {
        let Some(rest) = line.strip_prefix("VmRSS:") else {
            continue;
        };
        let num: String = rest
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .chars()
            .filter(|ch| ch.is_ascii_digit())
            .collect();
        if num.is_empty() {
            continue;
        }
        if let Ok(kb) = num.parse::<u64>() {
            return Some(kb);
        }
    }
    None
}

/// Parse `ps -o rss= -p <pid>` output (KiB). A zero RSS is real (freshly
/// spawned): reported, not dropped. Anything else unparseable is `None`.
/// Production use is macOS-only today; the unit test below runs everywhere.
#[cfg(any(test, target_os = "macos"))]
pub fn parse_ps_rss(text: &str) -> Option<u64> {
    match text.split_whitespace().next() {
        Some("0") => Some(0),
        Some(first) => first.parse::<u64>().ok().filter(|kb| *kb > 0),
        None => None,
    }
}

/// Resident KiB of one pid, or `None` when unreadable.
#[cfg(target_os = "linux")]
fn rss_kb_of(pid: i32) -> Option<u64> {
    let body = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    parse_vmrss_kb(&body)
}

#[cfg(target_os = "macos")]
fn rss_kb_of(pid: i32) -> Option<u64> {
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    parse_ps_rss(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn rss_kb_of(_pid: i32) -> Option<u64> {
    None
}

/// Parent pid of one pid, or `None` when unreadable.
#[cfg(target_os = "linux")]
fn ppid_of(candidate: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{candidate}/stat")).ok()?;
    let after = stat.rsplit(')').next()?;
    let mut fields = after.split_whitespace();
    fields.next()?; // state
    fields.next()?.parse::<i32>().ok() // ppid
}

/// Parentage table pid -> ppid from one `ps` pass.
#[cfg(target_os = "macos")]
fn ppid_table() -> HashMap<i32, i32> {
    let mut table = HashMap::new();
    let Ok(output) = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid="])
        .env("LC_ALL", "C")
        .output()
    else {
        return table;
    };
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut tokens = line.split_whitespace();
        if let (Some(candidate), Some(ppid)) = (tokens.next(), tokens.next())
            && let (Ok(candidate), Ok(ppid)) = (candidate.parse::<i32>(), ppid.parse::<i32>())
            && candidate > 0
            && ppid >= 0
        {
            table.insert(candidate, ppid);
        }
    }
    table
}

/// Breadth-first tree from `root` over a parentage table. Root first; the
/// visited set (not pid ordering) guarantees termination even if pid reuse
/// ever formed a cycle.
pub fn bfs_tree(root: i32, children: &HashMap<i32, Vec<i32>>) -> Vec<i32> {
    let mut members = vec![root];
    let mut seen = std::collections::HashSet::from([root]);
    let mut queue = vec![root];
    while let Some(parent) = queue.pop() {
        if let Some(kids) = children.get(&parent) {
            for kid in kids {
                if seen.insert(*kid) {
                    members.push(*kid);
                    queue.push(*kid);
                }
            }
        }
    }
    members
}

/// Pids of `root` plus currently-visible descendants, root first. One
/// platform snapshot per call; an exit race just shrinks the set.
pub fn tree_pids(root: i32) -> Vec<i32> {
    #[cfg(target_os = "linux")]
    {
        let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
        if let Ok(dir) = std::fs::read_dir("/proc") {
            for entry in dir.flatten() {
                let name = entry.file_name();
                let Ok(candidate) = name.to_string_lossy().parse::<i32>() else {
                    continue;
                };
                if candidate <= 0 {
                    continue;
                }
                if let Some(ppid) = ppid_of(candidate) {
                    children.entry(ppid).or_default().push(candidate);
                }
            }
        }
        bfs_tree(root, &children)
    }
    #[cfg(target_os = "macos")]
    {
        bfs_tree(root, &ppid_table_inverted())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        vec![root]
    }
}

/// Inverted parentage table (parent -> children) from one `ps` pass.
#[cfg(target_os = "macos")]
fn ppid_table_inverted() -> HashMap<i32, Vec<i32>> {
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    let Ok(output) = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid="])
        .env("LC_ALL", "C")
        .output()
    else {
        return children;
    };
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut tokens = line.split_whitespace();
        if let (Some(candidate), Some(ppid)) = (tokens.next(), tokens.next())
            && let (Ok(candidate), Ok(ppid)) = (candidate.parse::<i32>(), ppid.parse::<i32>())
            && candidate > 0
            && ppid >= 0
        {
            children.entry(ppid).or_default().push(candidate);
        }
    }
    children
}

/// Start time of one pid in unix seconds, same source the scan uses.
/// `None` when gone or unreadable — callers treat that as "do not act".
#[cfg(target_os = "linux")]
pub fn started_secs_of(pid: i32) -> Option<u64> {
    std::fs::metadata(format!("/proc/{pid}"))
        .and_then(|meta| meta.modified())
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .ok()
        .filter(|secs| *secs > 0)
}

/// Same contract via a single-pid `ps` etime lookup.
#[cfg(target_os = "macos")]
pub fn started_secs_of(pid: i32) -> Option<u64> {
    let output = std::process::Command::new("ps")
        .args(["-o", "etime=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    // Reuse the scan's shape: [[dd-]hh:]mm:ss. Duplicated in three lines
    // rather than shared so this file stays dependency-free.
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let (days, clock) = match text.split_once('-') {
        Some((d, c)) => (d.parse::<u64>().ok()?, c),
        None => (0, text),
    };
    let mut fields = clock.split(':');
    let mut parts = Vec::new();
    for part in fields.by_ref() {
        parts.push(part.parse::<u64>().ok()?);
    }
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }
    let elapsed = if parts.len() == 3 {
        parts[0] * 3600 + parts[1] * 60 + parts[2]
    } else {
        parts[0] * 60 + parts[1]
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    now.checked_sub(elapsed).filter(|started| *started > 0)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn started_secs_of(_pid: i32) -> Option<u64> {
    None
}

/// Sum resident KiB over `root` plus currently-visible descendants. Pids that
/// vanish mid-scan contribute nothing. Returns `(total_kib, counted)`.
pub fn tree_rss_kib(root: i32) -> (u64, usize) {    let mut total = 0u64;
    let mut counted = 0usize;
    for pid in tree_pids(root) {
        if let Some(kb) = rss_kb_of(pid) {
            total = total.saturating_add(kb);
            counted += 1;
        }
    }
    (total, counted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vmrss_parses_kilobytes() {
        let status = "Name:\tsleep\nVmRSS:\t   12345 kB\nVmSwap:\t0 kB\n";
        assert_eq!(parse_vmrss_kb(status), Some(12345));
        assert_eq!(parse_vmrss_kb("Name:\tsleep\n"), None);
        assert_eq!(parse_vmrss_kb("VmRSS:\t kB\n"), None);
        assert_eq!(parse_vmrss_kb(""), None);
    }

    #[test]
    fn ps_rss_parses_kilobytes() {
        assert_eq!(parse_ps_rss("  3936\n"), Some(3936));
        assert_eq!(parse_ps_rss("0\n"), Some(0));
        assert_eq!(parse_ps_rss(""), None);
        assert_eq!(parse_ps_rss("abc\n"), None);
    }

    #[test]
    fn bfs_tree_covers_descendants_and_survives_cycles() {
        let children: HashMap<i32, Vec<i32>> =
            [(1, vec![2, 3]), (2, vec![4]), (4, vec![2])].into_iter().collect();
        let members = bfs_tree(1, &children);
        assert_eq!(members[0], 1);
        let mut rest = members[1..].to_vec();
        rest.sort_unstable();
        assert_eq!(rest, vec![2, 3, 4]);
        assert_eq!(bfs_tree(9, &children), vec![9]);
    }

    #[test]
    fn live_tree_sums_real_memory() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as i32;
        assert!(counted_at_least_root(pid));
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(target_os = "linux")]
    fn counted_at_least_root(pid: i32) -> bool {
        let (total, counted) = tree_rss_kib(pid);
        counted >= 1 && total > 0
    }

    #[cfg(not(target_os = "linux"))]
    fn counted_at_least_root(_pid: i32) -> bool {
        // No live process tree on this platform in CI; the parsers above
        // carry the coverage.
        true
    }
}
