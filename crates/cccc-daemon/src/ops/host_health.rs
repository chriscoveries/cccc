//! `group_health`: how much RAM a Group holds, and how loaded the host is.
//!
//! Display only, like the per-Actor reading it builds on. The Group's figure is
//! the sum of the readings `actor_memory` already holds for its running Actors;
//! the host figures are three small `/proc` reads taken when an Operator asks,
//! not on a timer of their own, so a header that is not open costs nothing.
//!
//! Everything here is either measured or absent. A figure the kernel would not
//! give is `null`, never `0`: an Operator reading `0` free memory would take a
//! healthy host for a dying one. Nothing in delivery, idling, dispatch or health
//! reads these numbers, and nothing stops an Actor because of them — the Stop
//! button in the pulldown is the existing stop operation, pressed by a person.

use serde_json::{Map, Value};
use std::path::Path;

const MIB: u64 = 1024;

/// What the host said, any of which may be absent.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct HostReading {
    pub mem_total_mib: Option<u64>,
    pub mem_available_mib: Option<u64>,
    pub load_1m: Option<f64>,
    pub load_5m: Option<f64>,
    pub load_15m: Option<f64>,
    pub cpu_count: Option<u32>,
}

/// Read the host's memory, load and CPU count under `root` (a `/proc`-shaped
/// directory). Each file that cannot be read leaves only its own fields absent.
pub(crate) fn read_host(root: &Path) -> HostReading {
    let meminfo = std::fs::read_to_string(root.join("meminfo")).unwrap_or_default();
    let (mem_total_mib, mem_available_mib) = parse_meminfo(&meminfo);
    let loadavg = std::fs::read_to_string(root.join("loadavg")).unwrap_or_default();
    let (load_1m, load_5m, load_15m) = parse_loadavg(&loadavg);
    let stat = std::fs::read_to_string(root.join("stat")).unwrap_or_default();
    HostReading {
        mem_total_mib,
        mem_available_mib,
        load_1m,
        load_5m,
        load_15m,
        cpu_count: parse_cpu_count(&stat),
    }
}

/// `MemTotal` and `MemAvailable` in MiB. Available is capped at total, so a
/// reader never sees more free memory than the host has.
pub(crate) fn parse_meminfo(text: &str) -> (Option<u64>, Option<u64>) {
    let total = parse_kib(text, "MemTotal:");
    let available = parse_kib(text, "MemAvailable:");
    let total = total.map(|kib| kib / MIB);
    let available = match (available, total) {
        (Some(free), Some(total)) => Some((free / MIB).min(total)),
        (Some(free), None) => Some(free / MIB),
        (None, _) => None,
    };
    (total, available)
}

/// The three load averages. A line that does not carry three numbers is absent
/// rather than partly filled in.
pub(crate) fn parse_loadavg(text: &str) -> (Option<f64>, Option<f64>, Option<f64>) {
    let mut fields = text.split_whitespace();
    let parse = |value: Option<&str>| value.and_then(|value| value.parse::<f64>().ok());
    let one = parse(fields.next());
    let five = parse(fields.next());
    let fifteen = parse(fields.next());
    match (one, five, fifteen) {
        (Some(one), Some(five), Some(fifteen)) => (Some(one), Some(five), Some(fifteen)),
        _ => (None, None, None),
    }
}

/// Online CPUs, counted from the per-CPU lines of `/proc/stat`. This is the
/// number the load average is divided by, so it is the number the chip shows.
pub(crate) fn parse_cpu_count(stat: &str) -> Option<u32> {
    let mut count = 0u32;
    for line in stat.lines() {
        let Some(rest) = line.strip_prefix("cpu") else {
            continue;
        };
        // `cpu0`, `cpu1`, … carry the number against the name; the aggregate
        // line is `cpu` followed by its totals, and is not a CPU.
        if !rest.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        let name = rest.split_whitespace().next().unwrap_or_default();
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_digit()) {
            count += 1;
        }
    }
    (count > 0).then_some(count)
}

/// The `group_health` result: the Group's own total from the readings the
/// sampler holds, plus the host figures read now.
pub(crate) fn report(root: &Path, group_id: &str, actors_running: u64) -> Value {
    let (rss_mib, pss_mib, sampled_actors) = super::actor_memory::group_totals(group_id);
    let host = read_host(root);
    let mut object = Map::new();
    object.insert("group_id".into(), Value::from(group_id));
    object.insert("rss_mib".into(), number(rss_mib));
    object.insert("pss_mib".into(), number(pss_mib));
    object.insert("actors_running".into(), Value::from(actors_running));
    object.insert("actors_sampled".into(), Value::from(sampled_actors));
    object.insert("host_mem_total_mib".into(), number(host.mem_total_mib));
    object.insert(
        "host_mem_available_mib".into(),
        number(host.mem_available_mib),
    );
    object.insert("host_loadavg_1m".into(), decimal(host.load_1m));
    object.insert("host_loadavg_5m".into(), decimal(host.load_5m));
    object.insert("host_loadavg_15m".into(), decimal(host.load_15m));
    object.insert(
        "host_cpu_count".into(),
        host.cpu_count.map_or(Value::Null, Value::from),
    );
    object.insert("sampled_at".into(), Value::from(cccc_contracts::utc_now()));
    Value::Object(object)
}

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

/// Two decimals is what `top` shows for load, and what the chip prints.
fn decimal(value: Option<f64>) -> Value {
    value.map_or(Value::Null, |value| {
        Value::from((value * 100.0).round() / 100.0)
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn fixture() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/actor-health")
    }

    /// Captured from this host on 2026-10-22 (see the fixture README):
    /// MemTotal 32089156 kB, MemAvailable 19481288 kB, load 8.17 5.76 5.47,
    /// 16 online CPUs.
    #[test]
    fn reads_the_hosts_real_memory_load_and_cpu_count() {
        let host = read_host(&fixture());
        assert_eq!(host.mem_total_mib, Some(32_089_156 / MIB));
        assert_eq!(host.mem_available_mib, Some(19_481_288 / MIB));
        assert_eq!(host.load_1m, Some(8.17));
        assert_eq!(host.load_5m, Some(5.76));
        assert_eq!(host.load_15m, Some(5.47));
        assert_eq!(host.cpu_count, Some(16));
    }

    #[test]
    fn available_memory_is_never_more_than_total() {
        let text = "MemTotal:       1000 kB\nMemAvailable:   4000 kB\n";
        let (total, available) = parse_meminfo(text);
        assert_eq!(total, Some(0), "1000 kB is under one MiB and rounds down");
        assert_eq!(available, Some(0), "capped at the total");
    }

    #[test]
    fn an_unreadable_host_file_leaves_its_own_fields_absent() {
        let empty = tempfile::tempdir().expect("tempdir");
        let host = read_host(empty.path());
        assert_eq!(host.mem_total_mib, None, "no meminfo, no figure");
        assert_eq!(host.mem_available_mib, None);
        assert_eq!(host.load_1m, None, "no loadavg, no figure");
        assert_eq!(host.cpu_count, None);
    }

    #[test]
    fn a_truncated_load_average_is_absent_rather_than_part_filled() {
        assert_eq!(parse_loadavg("8.17 5.76\n"), (None, None, None));
        assert_eq!(parse_loadavg(""), (None, None, None));
        assert_eq!(
            parse_loadavg("0.00 0.01 0.05 1/3511 1774388"),
            (Some(0.0), Some(0.01), Some(0.05)),
            "a quiet host is zero, not absent"
        );
    }

    #[test]
    fn the_aggregate_line_is_not_counted_as_a_cpu() {
        let stat = "cpu  1 2 3\ncpu0 1 2 3\ncpu1 1 2 3\nintr 5\n";
        assert_eq!(parse_cpu_count(stat), Some(2));
        assert_eq!(parse_cpu_count("intr 5\n"), None);
    }

    #[test]
    fn a_group_with_no_readings_reports_no_total_rather_than_zero() {
        let report = report(&fixture(), "no-such-group", 0);
        assert_eq!(report["rss_mib"], Value::Null);
        assert_eq!(report["pss_mib"], Value::Null);
        assert_eq!(report["actors_sampled"], 0);
        assert_eq!(report["actors_running"], 0);
        assert!(report["host_mem_total_mib"].as_u64().is_some());
        assert!(report["host_cpu_count"].as_u64().is_some());
        assert!(
            report["sampled_at"]
                .as_str()
                .is_some_and(|at| at.ends_with('Z'))
        );
    }
}
