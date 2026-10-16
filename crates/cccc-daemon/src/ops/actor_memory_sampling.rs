//! Process-tree accounted memory: Linux VmRSS+VmSwap, Darwin physical footprint.
//! Incomplete Darwin captures are unreadable, never RSS-only or partial totals.
//! Actual provider ownership/generation is checked by the maintenance coordinator.

use std::collections::HashMap;

#[cfg(any(test, target_os = "macos"))]
mod macos;

pub fn accounting_basis() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        "linux_rss_plus_swap"
    }
    #[cfg(target_os = "macos")]
    {
        macos::ACCOUNTING_BASIS
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        "unsupported"
    }
}

/// Async observation budget, excluding the existing owned-command cleanup.
pub fn accounting_budget_ms() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        u64::try_from(macos::TREE_TIMEOUT.as_millis()).ok()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

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

/// Linux resident-plus-swap KiB of one pid, or `None` when unreadable.
#[cfg(target_os = "linux")]
fn rss_kb_of(pid: i32) -> Option<u64> {
    let body = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    parse_vmrss_kb(&body)
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

/// Linux root plus currently visible descendants, root first.
#[cfg(target_os = "linux")]
pub fn tree_pids(root: i32) -> Vec<i32> {
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

/// Linux retains its existing resident+swap accounting. Darwin requires a
/// bounded non-atomic pass over the observed physical-footprint tree. (0,0)
/// means unreadable/unsupported, not a valid zero footprint. The enforcement
/// caller skips counted==0; root ownership/idle authority is kept separately.
#[cfg(target_os = "linux")]
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

#[cfg(target_os = "macos")]
pub fn tree_rss_kib(root: i32) -> (u64, usize) {
    super::local_headless::run_managed_launch(async move { macos::tree_footprint_kib(root).await })
        .unwrap_or((0, 0))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn tree_rss_kib(_root: i32) -> (u64, usize) {
    (0, 0)
}

/// Measure the union of observed process trees, without counting a provider
/// twice when its owned helper is already a descendant of another root.
/// This is non-atomic accounting, never process ownership or stop authority.
/// Invalid/unsupported roots and unreadable observations return count zero.
#[cfg(target_os = "linux")]
pub fn roots_rss_kib(roots: &[u32]) -> (u64, usize) {
    let Some(roots) = checked_roots(roots) else {
        return (0, 0);
    };
    let mut members = std::collections::BTreeSet::new();
    for root in roots {
        members.extend(tree_pids(root));
    }
    let mut total = 0_u64;
    let mut counted = 0_usize;
    for pid in members {
        if let Some(kib) = rss_kb_of(pid) {
            let Some(sum) = total.checked_add(kib) else {
                return (0, 0);
            };
            total = sum;
            counted += 1;
        }
    }
    (total, counted)
}

#[cfg(target_os = "macos")]
pub fn roots_rss_kib(roots: &[u32]) -> (u64, usize) {
    let Some(roots) = checked_roots(roots) else {
        return (0, 0);
    };
    super::local_headless::run_managed_launch(
        async move { macos::roots_footprint_kib(roots).await },
    )
    .unwrap_or((0, 0))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn roots_rss_kib(_roots: &[u32]) -> (u64, usize) {
    (0, 0)
}

#[cfg(any(test, target_os = "linux", target_os = "macos"))]
fn checked_roots(roots: &[u32]) -> Option<Vec<i32>> {
    if roots.is_empty() || roots.len() > 128 {
        return None;
    }
    roots
        .iter()
        .map(|pid| i32::try_from(*pid).ok().filter(|pid| *pid > 1))
        .collect()
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
    fn observation_roots_reject_empty_init_overflow_and_unbounded_inputs() {
        for roots in [vec![], vec![0], vec![1], vec![u32::MAX], vec![2; 129]] {
            assert_eq!(checked_roots(&roots), None, "{roots:?}");
        }
        assert_eq!(checked_roots(&[41, 42, 41]), Some(vec![41, 42, 41]));
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

    #[cfg(target_os = "linux")]
    #[test]
    fn known_unreadable_zombie_member_cannot_be_hidden_by_a_readable_owned_parent() {
        use std::path::PathBuf;
        use std::process::{Child, Command, ExitStatus, Stdio};
        use std::time::{Duration, Instant};

        struct OwnedFixture {
            child: Child,
            release: PathBuf,
        }
        impl OwnedFixture {
            fn finish(&mut self) -> std::io::Result<ExitStatus> {
                std::fs::write(&self.release, b"release")?;
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    if let Some(status) = self.child.try_wait()? {
                        return Ok(status);
                    }
                    if Instant::now() >= deadline {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "owned zombie fixture did not reap and exit",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        impl Drop for OwnedFixture {
            fn drop(&mut self) {
                if self.finish().is_err() && self.child.kill().is_ok() {
                    // Wait only after successful termination of our exact Child.
                    let _ = self.child.wait();
                }
            }
        }

        let private = tempfile::tempdir().expect("private fixture directory");
        let ready = private.path().join("ready");
        let release = private.path().join("release");
        let reaped = private.path().join("reaped");
        let program = r#"
import os, pathlib, sys, time
ready, release, reaped = map(pathlib.Path, sys.argv[1:])
keep = bytearray(8 * 1024 * 1024)
for offset in range(0, len(keep), 4096):
    keep[offset] = 1
kid = os.fork()
if kid == 0:
    os._exit(0)
try:
    deadline = time.monotonic() + 5
    while True:
        stat = pathlib.Path('/proc', str(kid), 'stat').read_text()
        if stat.rsplit(')', 1)[1].split()[0] == 'Z':
            break
        if time.monotonic() >= deadline:
            raise RuntimeError('owned child did not become a retained zombie')
        time.sleep(0.01)
    ready.write_text(str(kid))
    deadline = time.monotonic() + 20
    while not release.exists() and time.monotonic() < deadline:
        time.sleep(0.01)
finally:
    waited, _ = os.waitpid(kid, 0)
    reaped.write_text(str(waited))
"#;
        let child = Command::new("python3")
            .args(["-u", "-c", program])
            .arg(&ready)
            .arg(&release)
            .arg(&reaped)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn bounded owned process tree");
        let root = child.id();
        let mut fixture = OwnedFixture { child, release };
        let observations = (|| -> std::io::Result<_> {
            let deadline = Instant::now() + Duration::from_secs(8);
            let zombie = loop {
                if let Ok(text) = std::fs::read_to_string(&ready)
                    && let Ok(pid) = text.parse::<u32>()
                {
                    break pid;
                }
                if fixture.child.try_wait()?.is_some() || Instant::now() >= deadline {
                    return Err(std::io::Error::other("owned fixture did not become ready"));
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            let root = i32::try_from(root).map_err(std::io::Error::other)?;
            let zombie_root = i32::try_from(zombie).map_err(std::io::Error::other)?;
            let parent_kib = rss_kb_of(root);
            let zombie_kib = rss_kb_of(zombie_root);
            let members = tree_pids(root);
            let single = tree_rss_kib(root);
            let union =
                roots_rss_kib(&[u32::try_from(root).map_err(std::io::Error::other)?, zombie]);
            Ok((
                zombie,
                parent_kib,
                zombie_kib,
                members.contains(&zombie_root),
                single,
                union,
            ))
        })();
        // Reap the retained child and our owned root before any evidence assertion,
        // including readiness/sample failures. Drop also retries bounded cleanup.
        let stopped = fixture.finish();
        let reaped_pid = std::fs::read_to_string(&reaped);
        drop(fixture);
        assert!(stopped.expect("owned root cleanup").success());
        let (zombie, parent_kib, zombie_kib, included, single, union) =
            observations.expect("observe actual retained zombie tree");
        assert_eq!(reaped_pid.expect("child was reaped"), zombie.to_string());
        assert!(
            parent_kib.is_some_and(|kib| kib > 0),
            "owned parent is readable"
        );
        assert_eq!(zombie_kib, None, "known child lacks RSS+swap counters");
        assert!(
            included,
            "the unreadable child was in the observed parent tree"
        );
        assert_eq!(
            (single, union),
            ((0, 0), (0, 0)),
            "neither single-root nor union sampling may advertise a partial total"
        );
    }
}
