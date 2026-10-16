//! Darwin enforcement basis: current physical footprint, not RSS+swap bytes.
//! Apple: https://developer.apple.com/videos/play/wwdc2022/10106/ (7:28,20:50).
//! Dirty, compressed and swapped pages are included; reclaimable clean pages
//! are excluded. Never add RSS or VmSwap to the footprint (double counting).
//! Synthetic parser tests are not actual macOS capture validation.
use std::io;

const VMMAP_LIMIT: usize = 64 * 1024;
pub(super) const ACCOUNTING_BASIS: &str = "macos_physical_footprint";

/// Parse only the current footprint, excluding the separately labelled peak.
/// vmmap's K/M/G/T suffixes are interpreted as binary units; real-runner output
/// must confirm this grammar before this adapter is enabled for supported OSes.
fn parse_current_footprint_kib(stdout: &str) -> Option<u64> {
    if stdout.len() > VMMAP_LIMIT || stdout.contains('\0') {
        return None;
    }
    let mut values = stdout
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("Physical footprint:"));
    let current = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let mut fields = current.split_whitespace();
    let token = fields.next()?;
    if fields.next().is_some() {
        return None;
    }
    let (number, unit_bytes) = match token.as_bytes().last()? {
        b'B' => (&token[..token.len() - 1], 1_u128),
        b'K' => (&token[..token.len() - 1], 1_u128 << 10),
        b'M' => (&token[..token.len() - 1], 1_u128 << 20),
        b'G' => (&token[..token.len() - 1], 1_u128 << 30),
        b'T' => (&token[..token.len() - 1], 1_u128 << 40),
        _ => return None,
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.len() > 6
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        || (number.contains('.') && fraction.is_empty())
    {
        return None;
    }
    let denominator = 10_u128.checked_pow(u32::try_from(fraction.len()).ok()?)?;
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u128>().ok()?
    };
    let scaled = whole
        .parse::<u128>()
        .ok()?
        .checked_mul(denominator)?
        .checked_add(fraction)?;
    let numerator = scaled.checked_mul(unit_bytes)?;
    let denominator_kib = denominator.checked_mul(1024)?;
    let kib = numerator
        .checked_add(denominator_kib - 1)?
        .checked_div(denominator_kib)?;
    u64::try_from(kib).ok()
}

fn parse_successful_capture(
    stdout: &[u8],
    stderr: &[u8],
    success: bool,
    stdout_truncated: bool,
    stderr_truncated: bool,
) -> io::Result<u64> {
    if !stderr.is_empty()
        || !success
        || stdout_truncated
        || stderr_truncated
        || stdout.len() > VMMAP_LIMIT
    {
        return Err(io::Error::other(
            "vmmap did not return a complete successful sample",
        ));
    }
    let text =
        std::str::from_utf8(stdout).map_err(|_| io::Error::other("vmmap stdout was not UTF-8"))?;
    parse_current_footprint_kib(text)
        .ok_or_else(|| io::Error::other("vmmap omitted a valid unique current physical footprint"))
}

#[cfg(target_os = "macos")]
const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
#[cfg(target_os = "macos")]
pub(super) const TREE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
#[cfg(target_os = "macos")]
const PARENTAGE_LIMIT: usize = 1024 * 1024;
const MAX_TREE_PIDS: usize = 128;

/// Parse a complete `ps -A -o pid=,ppid=` snapshot. A malformed or duplicate
/// row cannot support a complete owned-tree accounting result.
fn parse_parentage(text: &str) -> io::Result<std::collections::HashMap<i32, i32>> {
    let mut table = std::collections::HashMap::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let mut fields = line.split_whitespace();
        let pid = fields
            .next()
            .ok_or_else(|| io::Error::other("missing process PID"))?;
        let ppid = fields
            .next()
            .ok_or_else(|| io::Error::other("missing process PPID"))?;
        if fields.next().is_some()
            || !pid.bytes().all(|byte| byte.is_ascii_digit())
            || !ppid.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(io::Error::other("invalid process parentage row"));
        }
        let pid = pid.parse::<i32>().map_err(io::Error::other)?;
        let ppid = ppid.parse::<i32>().map_err(io::Error::other)?;
        if pid <= 0 || table.insert(pid, ppid).is_some() {
            return Err(io::Error::other("invalid or duplicate process PID"));
        }
    }
    if table.is_empty() {
        return Err(io::Error::other("process parentage snapshot was empty"));
    }
    Ok(table)
}

fn tree_members(root: i32, table: &std::collections::HashMap<i32, i32>) -> io::Result<Vec<i32>> {
    if root <= 0 || !table.contains_key(&root) {
        return Err(io::Error::other("owned process root missing from snapshot"));
    }
    let mut children = std::collections::HashMap::<i32, Vec<i32>>::new();
    for (&pid, &ppid) in table {
        children.entry(ppid).or_default().push(pid);
    }
    let mut members = super::bfs_tree(root, &children);
    if members.len() > MAX_TREE_PIDS {
        return Err(io::Error::other(
            "owned process tree exceeded sampling bound",
        ));
    }
    members.sort_unstable();
    Ok(members)
}

fn verify_same_observed_parentage(
    root: i32,
    before: &std::collections::HashMap<i32, i32>,
    after: &std::collections::HashMap<i32, i32>,
) -> io::Result<()> {
    let members = tree_members(root, before)?;
    if members != tree_members(root, after)?
        || members.iter().any(|pid| before.get(pid) != after.get(pid))
    {
        return Err(io::Error::other(
            "owned process tree changed during sampling",
        ));
    }
    Ok(())
}

/// Union only the processes visible in this snapshot. Transient processes and
/// descendant PID reuse between snapshots remain outside this observation.
fn observed_union(
    roots: &[i32],
    table: &std::collections::HashMap<i32, i32>,
) -> io::Result<Vec<i32>> {
    if roots.is_empty() || roots.len() > MAX_TREE_PIDS {
        return Err(io::Error::other("invalid observed process root count"));
    }
    let mut union = std::collections::BTreeSet::new();
    for &root in roots {
        union.extend(tree_members(root, table)?);
        if union.len() > MAX_TREE_PIDS {
            return Err(io::Error::other(
                "observed process union exceeded sampling bound",
            ));
        }
    }
    Ok(union.into_iter().collect())
}

#[cfg(target_os = "macos")]
fn command(program: &str) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    command.env_clear().env("LC_ALL", "C").env("LANG", "C");
    command
}

#[cfg(target_os = "macos")]
async fn capture_vmmap(pid: i32) -> io::Result<cccc_runtime::CapturedOutput> {
    if pid <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid owned PID",
        ));
    }
    cccc_runtime::capture_command(
        command("/usr/bin/vmmap").args(["-summary", &pid.to_string()]),
        None,
        COMMAND_TIMEOUT,
        VMMAP_LIMIT,
    )
    .await
}

#[cfg(target_os = "macos")]
async fn parentage() -> io::Result<std::collections::HashMap<i32, i32>> {
    let output = cccc_runtime::capture_command(
        command("/bin/ps").args(["-A", "-o", "pid=,ppid="]),
        None,
        COMMAND_TIMEOUT,
        PARENTAGE_LIMIT,
    )
    .await?;
    if !output.status.success()
        || !output.stderr.is_empty()
        || output.stdout_truncated
        || output.stderr_truncated
    {
        return Err(io::Error::other("process parentage capture was incomplete"));
    }
    let text = std::str::from_utf8(&output.stdout).map_err(io::Error::other)?;
    parse_parentage(text)
}

/// Account every member of the observed tree, or return unreadable on any
/// detected missing/failed/truncated sample or changed observed parentage.
/// This pass is non-atomic: two snapshots cannot prove descendant creation
/// identity or rule out transient activity between observations. The caller's
/// real root ownership/generation and idle gate remain separate authorities;
/// this sampler only measures its argument and never chooses a stop target.
#[cfg(target_os = "macos")]
pub(super) async fn tree_footprint_kib(root: i32) -> io::Result<(u64, usize)> {
    roots_footprint_kib(vec![root]).await
}

/// All roots share one observation deadline and each observed PID is queried
/// once. Neither summing overlapping trees nor resetting the budget per root
/// can inflate a sample or multiply its advertised asynchronous time bound.
#[cfg(target_os = "macos")]
pub(super) async fn roots_footprint_kib(roots: Vec<i32>) -> io::Result<(u64, usize)> {
    tokio::time::timeout(TREE_TIMEOUT, async move {
        let before = parentage().await?;
        let members = observed_union(&roots, &before)?;
        let mut total = 0_u64;
        for &pid in &members {
            let output = capture_vmmap(pid).await?;
            let kib = parse_successful_capture(
                &output.stdout,
                &output.stderr,
                output.status.success(),
                output.stdout_truncated,
                output.stderr_truncated,
            )?;
            total = total
                .checked_add(kib)
                .ok_or_else(|| io::Error::other("owned footprint sum overflowed"))?;
        }
        let after = parentage().await?;
        for root in roots {
            verify_same_observed_parentage(root, &before, &after)?;
        }
        Ok((total, members.len()))
    })
    .await
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "owned footprint tree sampling timed out",
        )
    })?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounting_basis_is_os_specific() {
        assert_eq!(ACCOUNTING_BASIS, "macos_physical_footprint");
    }

    #[test]
    fn current_footprint_does_not_use_peak_or_virtual_address_size() {
        let stdout = "Process: owned-fixture [41]\nVirtual address size: 800.0G\nPhysical footprint: 21.5M\nPhysical footprint (peak): 4.0G\n";
        assert_eq!(parse_current_footprint_kib(stdout), Some(22_016));
        assert_eq!(
            parse_current_footprint_kib("Physical footprint (peak): 4.0G\n"),
            None
        );
    }

    #[test]
    fn binary_units_fractional_boundaries_and_zero_are_explicit() {
        for (token, expected) in [
            ("0B", 0),
            ("1025B", 2),
            ("1K", 1),
            ("0.000001M", 1),
            ("4G", 4_194_304),
            ("4.000001G", 4_194_306),
        ] {
            assert_eq!(
                parse_current_footprint_kib(&format!("Physical footprint: {token}\n")),
                Some(expected)
            );
        }
    }

    #[test]
    fn ambiguity_malformed_units_nonfinite_values_and_overflow_are_unreadable() {
        for stdout in [
            "",
            "Physical footprint: 1M\nPhysical footprint: 2M\n",
            "Physical footprint: -1M\n",
            "Physical footprint: NaNM\n",
            "Physical footprint: infG\n",
            "Physical footprint: 1 MB\n",
            "Physical footprint: 1.2.3M\n",
            "Physical footprint: 1.M\n",
            "Physical footprint: 1.0000001M\n",
            "Physical footprint: 1M extra\n",
            "Physical footprint: 340282366920938463463374607431768211455T\n",
            "Physical footprint: 1M\0\n",
        ] {
            assert_eq!(parse_current_footprint_kib(stdout), None, "{stdout:?}");
        }
    }

    #[test]
    fn failed_or_truncated_capture_never_becomes_a_valid_zero_sample() {
        let stdout = b"Physical footprint: 1M\n";
        assert_eq!(
            parse_successful_capture(stdout, b"", true, false, false).expect("valid sample"),
            1024
        );
        for (success, out_cut, err_cut) in [
            (false, false, false),
            (true, true, false),
            (true, false, true),
        ] {
            assert!(parse_successful_capture(stdout, b"", success, out_cut, err_cut).is_err());
        }
        assert!(parse_successful_capture(stdout, b"warning", true, false, false).is_err());
        assert!(parse_successful_capture(b"\xff", b"", true, false, false).is_err());
        assert!(
            parse_successful_capture(&vec![b'x'; VMMAP_LIMIT + 1], b"", true, false, false)
                .is_err()
        );
    }
}

#[cfg(test)]
mod parentage_tests {
    use super::*;

    #[test]
    fn strict_parentage_requires_positive_unique_pids_and_exact_two_fields() {
        let table = parse_parentage(" 10 1\n 11 10\n12 11\n20 1\n").expect("complete ps rows");
        assert_eq!(
            tree_members(10, &table).expect("owned tree"),
            vec![10, 11, 12]
        );
        for invalid in [
            "",
            "10\n",
            "10 1 extra\n",
            "0 0\n",
            "-1 0\n",
            "10 +1\n",
            "10 1\n10 2\n",
            "2147483648 1\n",
        ] {
            assert!(parse_parentage(invalid).is_err(), "{invalid:?}");
        }
        assert!(tree_members(99, &table).is_err());
        assert!(tree_members(0, &table).is_err());
    }

    #[test]
    fn an_observed_missing_new_or_reparented_member_invalidates_the_sample() {
        let before = parse_parentage("10 1\n11 10\n12 11\n20 1\n").expect("before");
        for after in [
            "10 1\n11 10\n20 1\n",
            "10 1\n11 10\n12 11\n13 10\n20 1\n",
            "10 1\n11 10\n12 20\n20 1\n",
            "10 1\n11 10\n12 10\n20 1\n",
        ] {
            let after = parse_parentage(after).expect("after");
            assert!(verify_same_observed_parentage(10, &before, &after).is_err());
        }
        let unrelated_changed = parse_parentage("10 1\n11 10\n12 11\n21 1\n").expect("unrelated");
        verify_same_observed_parentage(10, &before, &unrelated_changed)
            .expect("unrelated processes do not alter observed tree");
    }

    #[test]
    fn a_tree_beyond_the_explicit_bound_is_unreadable() {
        let table = (1..=MAX_TREE_PIDS + 1)
            .map(|pid| (pid as i32, if pid == 1 { 0 } else { 1 }))
            .collect();
        assert!(tree_members(1, &table).is_err());
    }

    #[test]
    fn overlapping_and_disjoint_roots_form_one_bounded_observed_union() {
        let table = parse_parentage("10 1\n11 10\n12 11\n20 1\n21 20\n").expect("parentage");
        assert_eq!(
            observed_union(&[10, 11, 10], &table).expect("overlap"),
            vec![10, 11, 12]
        );
        assert_eq!(
            observed_union(&[10, 20], &table).expect("disjoint"),
            vec![10, 11, 12, 20, 21]
        );
        assert!(observed_union(&[], &table).is_err());
        assert!(observed_union(&[99], &table).is_err());
        assert!(observed_union(&[10; MAX_TREE_PIDS + 1], &table).is_err());
    }
}

#[cfg(all(test, target_os = "macos"))]
mod owned_capture_tests;
