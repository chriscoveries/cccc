//! Linux process observations for display. Incomplete reads are unavailable,
//! never zero. No command lines or environment values are retained.

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProcessStat {
    parent: u32,
    user_ticks: u64,
    system_ticks: u64,
    child_user_ticks: u64,
    child_system_ticks: u64,
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
        child_user_ticks: fields.get(13)?.parse().ok()?,
        child_system_ticks: fields.get(14)?.parse().ok()?,
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

#[cfg(target_os = "linux")]
pub(super) mod linux {
    use super::{ProcessStat, parse_process_stat};
    use std::collections::{BTreeMap, BTreeSet, HashMap};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, Instant};

    const PROCESS_LIMIT: usize = 32_768;
    const CACHE_LIFETIME: Duration = Duration::from_secs(60);
    /// One full-host scan may serve every client for this long, and a failed
    /// scan stays unavailable for just as long instead of being retried per
    /// request.
    const CAPTURE_INTERVAL: Duration = Duration::from_secs(5);

    /// Per-identity interval inputs at one endpoint: the process's own ticks
    /// and the ticks of the children it has already waited for.
    type TreeTicks = (u64, u64);

    struct Previous {
        at: Instant,
        tree: BTreeMap<(u32, u64), TreeTicks>,
    }

    fn previous() -> &'static Mutex<HashMap<(u32, u64), Previous>> {
        static PREVIOUS: OnceLock<Mutex<HashMap<(u32, u64), Previous>>> = OnceLock::new();
        PREVIOUS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// One full-host scan (or one recorded failure of it) shared between
    /// every client that arrives inside the same window.
    struct Shared<T> {
        attempted: Instant,
        result: Option<Arc<T>>,
    }

    /// Serve the recent result, including a recorded unavailability, to every
    /// client inside the window; the first client after the window performs
    /// the scan itself.
    fn shared_with<T>(
        shared: &mut Option<Shared<T>>,
        now: Instant,
        scan: impl FnOnce() -> Option<T>,
    ) -> Option<Arc<T>> {
        if let Some(recent) = shared
            && now.saturating_duration_since(recent.attempted) < CAPTURE_INTERVAL
        {
            return recent.result.clone();
        }
        let result = scan().map(Arc::new);
        *shared = Some(Shared {
            attempted: now,
            result: result.clone(),
        });
        result
    }

    /// The scan every resource request is served from: one full-host read
    /// per window, however many clients ask, and a failure stays unavailable
    /// for the same window instead of being retried per request.
    pub(crate) fn capture_shared() -> Option<Arc<Snapshot>> {
        static SHARED: OnceLock<Mutex<Option<Shared<Snapshot>>>> = OnceLock::new();
        let mut shared = SHARED.get_or_init(|| Mutex::new(None)).lock().ok()?;
        shared_with(&mut shared, Instant::now(), Snapshot::capture)
    }

    pub(crate) struct Snapshot {
        processes: HashMap<u32, ProcessStat>,
        children: HashMap<u32, Vec<u32>>,
        page_bytes: u64,
        ticks_per_second: u64,
        at: Instant,
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
            // Each endpoint totals its live members' own ticks plus the ticks
            // of the children they already waited for: work by a child that
            // starts and exits between the endpoints arrives in its reaper's
            // waited-for totals, while live descendants contribute their own
            // ticks exactly once.
            let mut rss = 0u64;
            let mut current: BTreeMap<(u32, u64), TreeTicks> = BTreeMap::new();
            let mut total = 0u64;
            for pid in included {
                let stat = self.processes.get(&pid)?;
                rss = rss.checked_add(stat.rss_pages.checked_mul(self.page_bytes)?)?;
                let own = stat.user_ticks.checked_add(stat.system_ticks)?;
                let waited = stat.child_user_ticks.checked_add(stat.child_system_ticks)?;
                total = total.checked_add(own)?.checked_add(waited)?;
                current.insert((pid, stat.started_ticks), (own, waited));
            }
            let mut cache = previous().lock().ok()?;
            cache.retain(|_, value| self.at.saturating_duration_since(value.at) <= CACHE_LIFETIME);
            let key = (root, root_stat.started_ticks);
            let cpu = cache.get(&key).and_then(|old| {
                let seconds = self.at.saturating_duration_since(old.at).as_secs_f64();
                if seconds < 1.0 {
                    return None;
                }
                // A member that vanished without a surviving relative
                // inheriting its waited-for accounting left this process
                // tree; that work cannot be attributed, so the value is
                // unavailable rather than partial.
                let mut vanished = 0u64;
                let mut inherited = 0u64;
                for (identity, &(own, waited)) in &old.tree {
                    match current.get(identity) {
                        Some(&(_, waited_now)) => {
                            inherited = inherited.checked_add(waited_now.checked_sub(waited)?)?;
                        }
                        None => {
                            vanished = vanished.checked_add(own)?.checked_add(waited)?;
                        }
                    }
                }
                if inherited < vanished {
                    return None;
                }
                let before = old.tree.values().try_fold(0u64, |sum, (own, waited)| {
                    sum.checked_add(*own)?.checked_add(*waited)
                })?;
                let delta = total.checked_sub(before)?;
                Some(100.0 * delta as f64 / self.ticks_per_second as f64 / seconds)
            });
            // Very frequent concurrent requests must not prevent a usable interval.
            if cache.get(&key).is_none_or(|old| {
                self.at.saturating_duration_since(old.at) >= Duration::from_secs(1)
            }) {
                cache.insert(
                    key,
                    Previous {
                        at: self.at,
                        tree: current,
                    },
                );
            }
            Some((rss, cpu))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn captured_cpu_interval_uses_observed_ticks_and_elapsed_time() {
            // Control: the original captured pair of one real interpreter
            // across a measured busy interval, with equal identity sets and
            // no short-lived child; see the observed-child test below.
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

        #[test]
        fn an_observed_short_lived_child_is_in_the_interval() {
            // The captured pair above is the control: equal identity sets and
            // no short-lived child. This test adds a real one that starts,
            // burns CPU and is reaped entirely between the endpoints.
            let root = std::process::id();
            let first = Snapshot::capture().expect("first real capture");
            let (rss, initial) = first.actor(root).expect("own tree");
            assert!(rss > 0);
            assert!(initial.is_none(), "the first observation has no interval");

            let mut child = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg("while :; do :; done")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("burning child");
            let child_pid = child.id();
            let mut observed = 0u64;
            let deadline = Instant::now() + Duration::from_secs(10);
            while observed < 3 && Instant::now() < deadline {
                observed = std::fs::read_to_string(format!("/proc/{child_pid}/stat"))
                    .ok()
                    .and_then(|text| parse_process_stat(&text))
                    .map(|stat| stat.user_ticks + stat.system_ticks)
                    .unwrap_or(0);
                if observed == 0 {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            assert!(observed >= 3, "the child consumed observed CPU");
            child.kill().expect("stop the child");
            child.wait().expect("reap the child");
            // The reap moves the child's lifetime ticks into this process's
            // waited-for accounting, visible in its own /proc record.
            let own = std::fs::read_to_string(format!("/proc/{root}/stat")).expect("own record");
            let own = parse_process_stat(&own).expect("own parse");
            assert!(own.child_user_ticks + own.child_system_ticks >= observed);
            // Keep the interval above the one-second floor after the reap.
            let remaining = Duration::from_millis(1_300).saturating_sub(first.at.elapsed());
            std::thread::sleep(remaining);
            let second = Snapshot::capture().expect("second real capture");
            let (_, cpu) = second.actor(root).expect("own tree");
            let percent = cpu.expect("stable identity with covered attribution");
            let elapsed = second.at.saturating_duration_since(first.at).as_secs_f64();
            let accounted = percent / 100.0 * second.ticks_per_second as f64 * elapsed;
            assert!(
                accounted + 5.0 >= observed as f64,
                "the interval accounted {accounted} ticks for an observed child of {observed}"
            );
        }

        #[test]
        fn repeat_clients_are_served_the_recent_result_or_unavailability() {
            let now = Instant::now();
            let mut state: Option<Shared<u8>> = None;
            let mut scans = 0u8;
            let first = shared_with(&mut state, now, || {
                scans += 1;
                Some(7u8)
            })
            .expect("first scan");
            let second = shared_with(&mut state, now, || {
                scans += 1;
                Some(7u8)
            })
            .expect("recent result");
            let later = now + CAPTURE_INTERVAL + Duration::from_millis(1);
            let third = shared_with(&mut state, later, || {
                scans += 1;
                Some(9u8)
            })
            .expect("scan after the window");
            assert_eq!(scans, 2);
            assert!(Arc::ptr_eq(&first, &second));
            assert!(!Arc::ptr_eq(&first, &third));
            assert_eq!(*third, 9);

            // A failed scan is served as unavailable for the whole window
            // instead of starting another full-host read per request.
            let mut failed: Option<Shared<u8>> = None;
            let mut attempts = 0u8;
            assert!(
                shared_with(&mut failed, now, || {
                    attempts += 1;
                    None::<u8>
                })
                .is_none()
            );
            assert!(
                shared_with(&mut failed, now, || {
                    attempts += 1;
                    None::<u8>
                })
                .is_none()
            );
            assert_eq!(attempts, 1);
            assert!(
                shared_with(&mut failed, later, || {
                    attempts += 1;
                    None::<u8>
                })
                .is_none()
            );
            assert_eq!(attempts, 2);
        }

        #[test]
        fn concurrent_clients_share_one_real_capture() {
            let shared = Arc::new(Mutex::new(None));
            let start = Arc::new(std::sync::Barrier::new(8));
            let clients = (0..8)
                .map(|_| {
                    let shared = Arc::clone(&shared);
                    let start = Arc::clone(&start);
                    std::thread::spawn(move || {
                        start.wait();
                        let mut state = shared.lock().expect("shared lock");
                        shared_with(&mut state, Instant::now(), Snapshot::capture)
                    })
                })
                .collect::<Vec<_>>();
            let results = clients
                .into_iter()
                .map(|client| client.join().expect("client").expect("real capture"))
                .collect::<Vec<_>>();
            assert!(
                results
                    .iter()
                    .all(|result| Arc::ptr_eq(&results[0], result))
            );
        }
    }
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
        assert_eq!(stat.child_user_ticks, 0);
        assert_eq!(stat.child_system_ticks, 0);
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
