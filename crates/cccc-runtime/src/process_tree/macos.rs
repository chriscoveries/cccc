//! macOS peer-process scan.
//!
//! Linux reads `/proc/<pid>/environ` directly. macOS has no `/proc`, and the
//! precise route — `sysctl({CTL_KERN, KERN_PROCARGS2, pid})` plus
//! `proc_pidinfo` start times — needs raw FFI, which this workspace forbids
//! (`unsafe_code = "forbid"` in the root `Cargo.toml`, not overridable). So
//! the macOS scan goes through one `ps -Aeww -o pid=,etime=,command=` pass:
//! `eww` keeps the environment in the COMMAND column on macOS, and `etime`
//! gives elapsed time to derive start times from. One subprocess, no FFI,
//! and the parsers below are pure and unit-tested on every platform (this
//! path is proven on Linux CI with a stubbed `ps` output).

use super::leftover::TaggedProcess;

/// Parse a BSD `etime` elapsed field (`[[dd-]hh:]mm:ss`) into seconds.
/// Returns `None` on any other shape rather than guessing.
pub fn parse_etime(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let (days, clock) = match text.split_once('-') {
        Some((days, clock)) => (days.parse::<u64>().ok()?, clock),
        None => (0, text),
    };
    let mut parts = clock.split(':');
    let mut fields = Vec::new();
    for part in parts.by_ref() {
        fields.push(part.parse::<u64>().ok()?);
    }
    if fields.len() < 2 || fields.len() > 3 {
        return None;
    }
    let seconds = if fields.len() == 3 {
        fields[0] * 3600 + fields[1] * 60 + fields[2]
    } else {
        fields[0] * 60 + fields[1]
    };
    Some(days * 86400 + seconds)
}

/// Parse one `ps -Aeww -o pid=,pgid=,etime=,command=` line into
/// `(pid, pgid, elapsed_secs, comm, actor_id, group_id)`.
///
/// The COMMAND column mixes the command line and the environment; lane tags
/// are the `CCCC_*=` tokens. Lines without both tags are not attributable and
/// return `None`. A missing or unparsable pgid degrades to 0 (unknown), which
/// the reaper treats as "signal this pid only", never as a group.
pub fn parse_ps_scan_line(line: &str) -> Option<(i32, i32, u64, String, String, String)> {
    let mut tokens = line.split_whitespace();
    let pid: i32 = tokens.next()?.parse().ok()?;
    if pid <= 0 {
        return None;
    }
    let pgid: i32 = tokens
        .next()?
        .parse()
        .ok()
        .filter(|pgid| *pgid > 0)
        .unwrap_or(0);
    let elapsed = parse_etime(tokens.next()?)?;
    let rest: Vec<&str> = tokens.collect();
    let comm = rest
        .first()
        .and_then(|token| token.rsplit('/').next())
        .unwrap_or_default()
        .to_owned();
    let mut actor: Option<&str> = None;
    let mut group: Option<&str> = None;
    for token in &rest {
        let Some((key, value)) = token.split_once('=') else {
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
        (Some(actor), Some(group)) => Some((
            pid,
            pgid,
            elapsed,
            comm,
            actor.to_owned(),
            group.to_owned(),
        )),
        _ => None,
    }
}

/// Scan with an injected `ps` output supplier. Production passes a closure
/// running the real `ps`; tests pass canned output, which is how this path
/// is proven on Linux CI. `now_secs` anchors elapsed-to-start conversion.
pub fn scan_macos_with(now_secs: u64, ps_output: &dyn Fn() -> Option<String>) -> Vec<TaggedProcess> {
    let own_pid = std::process::id() as i32;
    let Some(output) = ps_output() else {
        return Vec::new();
    };
    let mut tagged = Vec::new();
    for line in output.lines() {
        let Some((pid, pgid, elapsed, comm, actor_id, group_id)) = parse_ps_scan_line(line)
        else {
            continue;
        };
        if pid == own_pid {
            continue;
        }
        tagged.push(TaggedProcess {
            pid,
            pgid,
            started_secs: now_secs.saturating_sub(elapsed),
            comm,
            actor_id,
            group_id,
        });
    }
    tagged.sort_by_key(|process| process.pid);
    tagged
}

/// Production macOS scan: one `ps` pass. `ps` failures (or a platform that
/// answers in another dialect) yield an empty vec, never a guess.
#[cfg(target_os = "macos")]
pub fn scan_macos() -> Vec<TaggedProcess> {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    scan_macos_with(now_secs, &|| {
        std::process::Command::new("ps")
            .args(["-Aeww", "-o", "pid=,pgid=,etime=,command="])
            .env("LC_ALL", "C")
            .output()
            .ok()
            .and_then(|output| String::from_utf8(output.stdout).ok())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etime_parses_bsd_shapes() {
        assert_eq!(parse_etime("00:05"), Some(5));
        assert_eq!(parse_etime("13:47:52"), Some(13 * 3600 + 47 * 60 + 52));
        assert_eq!(parse_etime("13-15:00:53"), Some(13 * 86400 + 15 * 3600 + 53));
        assert_eq!(parse_etime("1-00:00:01"), Some(86401));
        assert_eq!(parse_etime(""), None);
        assert_eq!(parse_etime("garbage"), None);
        assert_eq!(parse_etime("1:2:3:4"), None);
        assert_eq!(parse_etime("-15:00:53"), None);
    }

    #[test]
    fn ps_line_parser_finds_lane_tags() {
        let line = " 4261 4261 13-15:00:42 node /x/codex CCCC_ACTOR_ID=peer-a CCCC_GROUP_ID=g1 OTHER=1";
        assert_eq!(
            parse_ps_scan_line(line),
            Some((4261, 4261, 13 * 86400 + 15 * 3600 + 42, "node".into(), "peer-a".into(), "g1".into()))
        );
        // No tags: invisible.
        assert_eq!(parse_ps_scan_line("4261 4261 00:05 node --flag OTHER=1"), None);
        // Half-tagged: invisible.
        assert_eq!(
            parse_ps_scan_line("4261 4261 00:05 node CCCC_ACTOR_ID=a"),
            None
        );
        assert_eq!(parse_ps_scan_line("not-a-pid 9 00:05 node CCCC_ACTOR_ID=a CCCC_GROUP_ID=g"), None);
        assert_eq!(parse_ps_scan_line("0 0 00:05 node CCCC_ACTOR_ID=a CCCC_GROUP_ID=g"), None);
        assert_eq!(parse_ps_scan_line("4261 4261 bogus node CCCC_ACTOR_ID=a CCCC_GROUP_ID=g"), None);
        // Unparsable pgid degrades to 0 (single-pid signalling), not rejection.
        assert_eq!(
            parse_ps_scan_line("4261 ?? 00:05 node CCCC_ACTOR_ID=a CCCC_GROUP_ID=g"),
            Some((4261, 0, 5, "node".into(), "a".into(), "g".into()))
        );
    }

    #[test]
    fn stubbed_ps_output_drives_the_scan_without_syscalls() {
        let output = "  PID PGID ELAPSED COMMAND\n\
            1001 1001 00:10:00 sleep 600 CCCC_ACTOR_ID=a CCCC_GROUP_ID=g\n\
            1002 1002 00:00:05 node x OTHER=1\n".to_owned();
        let tagged = scan_macos_with(1_700_000_000, &|| Some(output.clone()));
        assert_eq!(tagged.len(), 1);
        assert_eq!(tagged[0].pid, 1001);
        assert_eq!(tagged[0].pgid, 1001);
        assert_eq!(tagged[0].actor_id, "a");
        assert_eq!(tagged[0].group_id, "g");
        assert_eq!(tagged[0].comm, "sleep");
        assert_eq!(tagged[0].started_secs, 1_700_000_000 - 600);
        // No ps output at all: empty, not an error.
        let empty: Vec<TaggedProcess> = scan_macos_with(0, &|| None);
        assert!(empty.is_empty());
    }
}
