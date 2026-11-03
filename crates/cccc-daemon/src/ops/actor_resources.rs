//! Linux process observations for display. Incomplete reads are unavailable,
//! never zero. No command lines or environment values are retained.

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProcessStat {
    parent: u32,
    user_ticks: u64,
    system_ticks: u64,
    started_ticks: u64,
    rss_pages: u64,
}

#[cfg(any(target_os = "linux", test))]
fn parse_process_stat(input: &str) -> Option<ProcessStat> {
    // The comm field is parenthesized and may itself contain spaces or `)`.
    let rest = input.rsplit_once(')')?.1.trim_start();
    let fields: Vec<&str> = rest.split_ascii_whitespace().collect();
    Some(ProcessStat {
        parent: fields.get(1)?.parse().ok()?,
        user_ticks: fields.get(11)?.parse().ok()?,
        system_ticks: fields.get(12)?.parse().ok()?,
        started_ticks: fields.get(19)?.parse().ok()?,
        rss_pages: fields.get(21)?.parse().ok()?,
    })
}

#[cfg(any(target_os = "linux", test))]
fn parse_memory(input: &str) -> Option<(u64, u64)> {
    fn field(input: &str, name: &str) -> Option<u64> {
        let mut parts = input
            .lines()
            .find_map(|line| line.strip_prefix(name))?
            .split_ascii_whitespace();
        let amount = parts.next()?.parse::<u64>().ok()?;
        (parts.next()? == "kB").then_some(amount.checked_mul(1024)?)
    }
    let total = field(input, "MemTotal:")?;
    let available = field(input, "MemAvailable:")?;
    (available <= total).then_some((total, available))
}

#[cfg(target_os = "linux")]
pub(super) fn host_memory() -> Option<(u64, u64)> {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|data| parse_memory(&data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_captured_process_and_memory() {
        let stat = parse_process_stat(include_str!("actor_resources_fixtures/proc-stat.txt"))
            .expect("captured Linux process stat");
        assert_eq!(stat.parent, 1_770_953);
        assert_eq!(stat.user_ticks, 2);
        assert_eq!(stat.system_ticks, 0);
        assert_eq!(stat.started_ticks, 376_655_417);
        assert_eq!(stat.rss_pages, 2_976);
        let (total, available) = parse_memory(include_str!("actor_resources_fixtures/meminfo.txt"))
            .expect("captured Linux memory summary");
        assert_eq!(total, 32_089_156 * 1024);
        assert_eq!(available, 16_869_080 * 1024);
    }

    #[test]
    fn truncated_captures_remain_unavailable() {
        // Truncations of the captured records, not new process observations.
        let stat = include_str!("actor_resources_fixtures/proc-stat.txt");
        assert!(parse_process_stat(stat.split_once(')').expect("captured comm").0).is_none());
        let memory = include_str!("actor_resources_fixtures/meminfo.txt");
        assert!(parse_memory(memory.lines().next().expect("captured first line")).is_none());
    }
}

#[cfg(target_os = "linux")]
pub(super) mod linux {
    use super::{ProcessStat, parse_process_stat};
    use std::collections::{BTreeMap, BTreeSet, HashMap};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    const PROCESS_LIMIT: usize = 32_768;
    const CACHE_LIFETIME: Duration = Duration::from_secs(60);

    struct Previous {
        at: Instant,
        ticks: BTreeMap<(u32, u64), u64>,
    }

    fn previous() -> &'static Mutex<HashMap<(u32, u64), Previous>> {
        static PREVIOUS: OnceLock<Mutex<HashMap<(u32, u64), Previous>>> = OnceLock::new();
        PREVIOUS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub(crate) struct Snapshot {
        processes: HashMap<u32, ProcessStat>,
        children: HashMap<u32, Vec<u32>>,
        page_bytes: u64,
        ticks_per_second: u64,
        at: Instant,
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn captured_cpu_interval_uses_observed_ticks_and_elapsed_time() {
            let first = include_str!("actor_resources_fixtures/cpu-first.txt");
            let second = include_str!("actor_resources_fixtures/cpu-second.txt");
            let pid: u32 = first
                .split_ascii_whitespace()
                .next()
                .expect("pid")
                .parse()
                .expect("captured pid");
            let initial = parse_process_stat(first).expect("first sample");
            let later = parse_process_stat(second).expect("second sample");
            assert_eq!(initial.started_ticks, later.started_ticks);
            let elapsed_ns: u64 = include_str!("actor_resources_fixtures/cpu-elapsed-ns.txt")
                .trim()
                .parse()
                .expect("captured elapsed time");
            let at = Instant::now();
            let a = Snapshot::new(HashMap::from([(pid, initial)]), 4096, 100, at);
            let b = Snapshot::new(
                HashMap::from([(pid, later)]),
                4096,
                100,
                at + Duration::from_nanos(elapsed_ns),
            );
            assert!(a.actor(pid).expect("first tree").1.is_none());
            let observed = b.actor(pid).expect("second tree").1.expect("CPU interval");
            let delta =
                later.user_ticks + later.system_ticks - initial.user_ticks - initial.system_ticks;
            let expected = 100.0 * delta as f64 / 100.0 / (elapsed_ns as f64 / 1_000_000_000.0);
            assert!((observed - expected).abs() < 1e-9);
            assert!(observed > 0.0);
        }

        #[test]
        fn captured_parent_and_child_are_counted_once() {
            let parent = include_str!("actor_resources_fixtures/tree-parent.txt");
            let child = include_str!("actor_resources_fixtures/tree-child.txt");
            let parent_pid: u32 = parent
                .split_ascii_whitespace()
                .next()
                .expect("pid")
                .parse()
                .expect("captured pid");
            let child_pid: u32 = child
                .split_ascii_whitespace()
                .next()
                .expect("pid")
                .parse()
                .expect("captured pid");
            let parent_stat = parse_process_stat(parent).expect("captured parent");
            let child_stat = parse_process_stat(child).expect("captured child");
            assert_eq!(child_stat.parent, parent_pid);
            let snapshot = Snapshot::new(
                HashMap::from([(parent_pid, parent_stat), (child_pid, child_stat)]),
                4096,
                100,
                Instant::now(),
            );
            let (rss, cpu) = snapshot.actor(parent_pid).expect("captured tree");
            assert_eq!(rss, (3104 + 2528) * 4096);
            assert!(cpu.is_none());
        }
    }

    impl Snapshot {
        fn new(
            processes: HashMap<u32, ProcessStat>,
            page_bytes: u64,
            ticks_per_second: u64,
            at: Instant,
        ) -> Self {
            let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
            for (&pid, stat) in &processes {
                children.entry(stat.parent).or_default().push(pid);
            }
            Self {
                processes,
                children,
                page_bytes,
                ticks_per_second,
                at,
            }
        }

        pub(crate) fn capture() -> Option<Self> {
            use nix::unistd::{SysconfVar, sysconf};
            let page_bytes = u64::try_from(sysconf(SysconfVar::PAGE_SIZE).ok()??).ok()?;
            let ticks_per_second = u64::try_from(sysconf(SysconfVar::CLK_TCK).ok()??).ok()?;
            if page_bytes == 0 || ticks_per_second == 0 {
                return None;
            }
            let mut processes = HashMap::new();
            for entry in std::fs::read_dir("/proc").ok()? {
                let entry = entry.ok()?;
                let Some(pid) = entry
                    .file_name()
                    .to_str()
                    .and_then(|s| s.parse::<u32>().ok())
                else {
                    continue;
                };
                if processes.len() >= PROCESS_LIMIT {
                    return None;
                }
                // Processes may disappear during a snapshot; non-disappearance
                // read failures are not a complete accounting of the tree.
                let stat = match std::fs::read_to_string(entry.path().join("stat")) {
                    Ok(stat) => stat,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(_) => return None,
                };
                processes.insert(pid, parse_process_stat(&stat)?);
            }
            Some(Self::new(
                processes,
                page_bytes,
                ticks_per_second,
                Instant::now(),
            ))
        }

        /// The snapshot and the current process must name the same kernel start.
        pub(crate) fn same_identity(&self, pid: u32) -> bool {
            let Some(stat) = self.processes.get(&pid) else {
                return false;
            };
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|text| parse_process_stat(&text))
                .is_some_and(|after| after.started_ticks == stat.started_ticks)
        }

        /// Recheck the root identity and its launch attribution before reporting.
        pub(crate) fn attributed(
            &self,
            pid: u32,
            home: &std::path::Path,
            group: &str,
            actor: &str,
        ) -> bool {
            use std::io::Read;
            let Ok(file) = std::fs::File::open(format!("/proc/{pid}/environ")) else {
                return false;
            };
            let mut bytes = Vec::new();
            if file.take(262_145).read_to_end(&mut bytes).is_err() || bytes.len() > 262_144 {
                return false;
            }
            let wanted: [(&[u8], &[u8]); 3] = [
                (b"CCCC_HOME", home.as_os_str().as_encoded_bytes()),
                (b"CCCC_GROUP_ID", group.as_bytes()),
                (b"CCCC_ACTOR_ID", actor.as_bytes()),
            ];
            for (key, value) in wanted {
                let matches: Vec<&[u8]> = bytes
                    .split(|&b| b == 0)
                    .filter_map(|entry| {
                        let separator = entry.iter().position(|&b| b == b'=')?;
                        let (name, rest) = entry.split_at(separator);
                        (name == key).then_some(&rest[1..])
                    })
                    .collect();
                if matches.as_slice() != [value] {
                    return false;
                }
            }
            self.same_identity(pid)
        }

        pub(crate) fn actor(&self, root: u32) -> Option<(u64, Option<f64>)> {
            let root_stat = self.processes.get(&root)?;
            let mut included = BTreeSet::new();
            let mut pending = vec![root];
            while let Some(pid) = pending.pop() {
                if included.insert(pid)
                    && let Some(descendants) = self.children.get(&pid)
                {
                    pending.extend(descendants);
                }
            }
            let mut rss = 0u64;
            let mut ticks = BTreeMap::new();
            for pid in included {
                let stat = self.processes.get(&pid)?;
                rss = rss.checked_add(stat.rss_pages.checked_mul(self.page_bytes)?)?;
                ticks.insert(
                    (pid, stat.started_ticks),
                    stat.user_ticks.checked_add(stat.system_ticks)?,
                );
            }
            let mut cache = previous().lock().ok()?;
            cache.retain(|_, value| self.at.saturating_duration_since(value.at) <= CACHE_LIFETIME);
            let key = (root, root_stat.started_ticks);
            let cpu = cache.get(&key).and_then(|old| {
                let seconds = self.at.saturating_duration_since(old.at).as_secs_f64();
                if seconds < 1.0 || old.ticks.keys().ne(ticks.keys()) {
                    return None;
                }
                let delta = ticks.iter().try_fold(0u64, |sum, (identity, &current)| {
                    sum.checked_add(current.checked_sub(*old.ticks.get(identity)?)?)
                })?;
                Some(100.0 * delta as f64 / self.ticks_per_second as f64 / seconds)
            });
            // Very frequent concurrent requests must not prevent a usable interval.
            if cache.get(&key).is_none_or(|old| {
                self.at.saturating_duration_since(old.at) >= Duration::from_secs(1)
            }) {
                cache.insert(key, Previous { at: self.at, ticks });
            }
            Some((rss, cpu))
        }
    }
}
