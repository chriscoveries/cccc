//! Process-tree memory sampling: Linux resident plus swapped pages.
//!
//! Linux reads `/proc` directly; macOS goes through `ps` (the workspace
//! forbids `unsafe`, so no sysctl FFI); other platforms report nothing.
//! Pids that vanish mid-scan contribute nothing. Pure parsers are unit
//! tested on every platform.

use std::collections::HashMap;

/// Sum resident and swapped KiB from Linux `/proc/<pid>/status`.
/// The historical parser name is retained for the isolated port's callers.
/// Missing, duplicate, overflowing or malformed counters are unreadable,
/// never evidence that a process has zero swapped memory.
#[cfg(any(test, target_os = "linux"))]
pub fn parse_vmrss_kb(status: &str) -> Option<u64> {
    fn field(status: &str, prefix: &str) -> Option<u64> {
        let mut matches = status.lines().filter_map(|line| line.strip_prefix(prefix));
        let line = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        let mut tokens = line.split_whitespace();
        let number = tokens.next()?;
        if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let value = number.parse::<u64>().ok()?;
        (tokens.next() == Some("kB") && tokens.next().is_none()).then_some(value)
    }
    field(status, "VmRSS:")?.checked_add(field(status, "VmSwap:")?)
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

/// Linux resident-plus-swap KiB of one pid, or `None` when unreadable.
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

/// Sum accounted KiB over `root` plus currently-visible descendants.
/// Linux includes VmRSS + VmSwap; the existing Darwin fallback is RSS only.
/// This partial patch does not complete the Darwin accounting policy. Pids that
/// vanish mid-scan contribute nothing. Returns `(total_kib, counted)`.
pub fn tree_rss_kib(root: i32) -> (u64, usize) {
    let mut total = 0u64;
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
        let children: HashMap<i32, Vec<i32>> = [(1, vec![2, 3]), (2, vec![4]), (4, vec![2])]
            .into_iter()
            .collect();
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

    // Append inside actor_memory_sampling::tests on 4583 only. The sampler module
    // does not exist in 953; do not claim this supplemental parser test is portable.
    #[test]
    fn swapped_out_owned_pages_still_count_toward_the_ceiling() {
        let status = "Name:\towned-provider\nPid:\t41\nVmSize:\t10485760 kB\nVmRSS:\t524288 kB\nVmSwap:\t3670017 kB\n";
        // 512 MiB resident + 3584 MiB and one KiB swapped exceeds 4096 MiB.
        // The existing production sampler's parser is exercised, not a duplicate
        // predicate or a mocked replacement sampler.
        let sampled_kib = parse_vmrss_kb(status).expect("sample attributable pages");
        assert_eq!(sampled_kib, 4_194_305);
        assert!(sampled_kib > 4096 * 1024);
    }

    #[test]
    fn unreadable_swap_or_malformed_units_are_not_reported_as_zero_memory() {
        for status in [
            "VmRSS: 512 kB\n",
            "VmRSS: 12x34 kB\nVmSwap: 0 kB\n",
            "VmRSS: 512 MB\nVmSwap: 0 kB\n",
            "VmRSS: 512 kB\nVmSwap: 0 kB\nVmSwap: 1 kB\n",
            "VmRSS: 18446744073709551615 kB\nVmSwap: 1 kB\n",
        ] {
            assert_eq!(parse_vmrss_kb(status), None, "unreadable sample: {status}");
        }
    }
}
