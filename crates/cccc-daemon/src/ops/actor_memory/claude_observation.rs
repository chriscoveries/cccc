//! Read-only attribution for the statically inspected 2.1.293 native metadata.
//! This is never an owned ChildOwner, signal target or provider idle fence.
#[cfg(any(target_os = "linux", test))]
use serde_json::Value;
use std::collections::BTreeMap;
use std::io;
use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Binding {
    pub short: String,
    pub session: String,
    pub host_pid: u32,
    pub host_start: String,
    pub repl_pid: u32,
    pub repl_start: String,
    pub attempt: u64,
    pub domain: String,
}

#[cfg(any(target_os = "linux", test))]
fn invalid() -> io::Error {
    io::Error::other("Claude 2.1.293 process metadata is missing, ambiguous or stale")
}

#[cfg(any(target_os = "linux", test))]
fn pid(value: &Value) -> io::Result<u32> {
    value
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n > 1)
        .ok_or_else(invalid)
}

#[cfg(any(target_os = "linux", test))]
fn text<'a>(value: &'a Value, key: &str) -> io::Result<&'a str> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(invalid)
}

#[cfg(any(target_os = "linux", test))]
fn token(value: &Value, key: &str) -> io::Result<String> {
    let s = text(value, key)?;
    if s.len() > 32 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    Ok(s.to_owned())
}

/// Pure metadata check; current process creation/domain evidence comes from
/// the caller, rather than trusting a provider-supplied pid as ownership.
#[allow(clippy::too_many_arguments)] // Explicit independent source/creation checks for this versioned metadata format.
#[cfg(any(target_os = "linux", test))]
fn parse(
    row: &Value,
    session: &Value,
    short: &str,
    expected_session: &str,
    expected_cwd: &Path,
    domain: &str,
    host_start: &str,
    repl_start: &str,
) -> io::Result<Binding> {
    let host_pid = pid(&row["pid"])?;
    let repl_pid = pid(&row["replPid"])?;
    let host = token(row, "procStart")?;
    let repl = token(row, "replProcStart")?;
    let session_start = token(session, "procStart")?;
    if text(row, "cliVersion")? != "2.1.293"
        || text(session, "version")? != "2.1.293"
        || text(row, "sessionId")? != expected_session
        || text(session, "sessionId")? != expected_session
        || pid(&session["pid"])? != repl_pid
        || text(session, "kind")? != "bg"
        || text(session, "jobId")? != short
        || text(session, "pidDomain")? != domain
        || host != host_start
        || repl != repl_start
        || session_start != repl
    {
        return Err(invalid());
    }
    let row_cwd = row
        .get("worktreePath")
        .filter(|v| v.is_string())
        .unwrap_or(&row["cwd"]);
    if Path::new(row_cwd.as_str().ok_or_else(invalid)?) != expected_cwd
        || Path::new(text(session, "cwd")?) != expected_cwd
    {
        return Err(invalid());
    }
    Ok(Binding {
        short: short.to_owned(),
        session: expected_session.to_owned(),
        host_pid,
        host_start: host,
        repl_pid,
        repl_start: repl,
        attempt: row["attempt"].as_u64().ok_or_else(invalid)?,
        domain: domain.to_owned(),
    })
}

#[cfg(target_os = "linux")]
fn creation(pid: u32) -> io::Result<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let (_, tail) = stat.rsplit_once(')').ok_or_else(invalid)?;
    // State is field 3; zero-based tail index 19 is proc stat field 22.
    let result = tail.split_whitespace().nth(19).ok_or_else(invalid)?;
    if result.is_empty() || !result.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    Ok(result.to_owned())
}

#[cfg(target_os = "linux")]
fn local_domain() -> io::Result<String> {
    let machine = std::fs::read_to_string("/etc/machine-id")?;
    let machine = machine.trim();
    if machine.len() != 32 || !machine.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    let namespace = std::fs::read_link("/proc/self/ns/pid")?;
    let namespace = namespace.to_str().ok_or_else(invalid)?;
    Ok(format!("linux:{machine}:{namespace}"))
}

#[cfg(target_os = "linux")]
fn small_json(path: &Path, maximum: u64, config: &Path) -> io::Result<Value> {
    use std::io::Read;
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > maximum {
        return Err(invalid());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != std::fs::metadata(config)?.uid()
            || metadata.permissions().mode() & 0o022 != 0
        {
            return Err(invalid());
        }
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(maximum + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        return Err(invalid());
    }
    serde_json::from_slice(&bytes).map_err(|_| invalid())
}

pub(super) fn observe(
    environment: &BTreeMap<String, String>,
    command: &[String],
    expected_session: &str,
    cwd: &Path,
) -> io::Result<Binding> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (environment, command, expected_session, cwd);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Claude process attribution requires a validated platform creation token (T649)",
        ))
    }
    #[cfg(target_os = "linux")]
    {
        let [_, verb, short] = command else {
            return Err(invalid());
        };
        if verb != "attach" || short.len() != 8 || !short.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(invalid());
        }
        let config = environment
            .get("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                environment
                    .get("HOME")
                    .or_else(|| environment.get("USERPROFILE"))
                    .map(|home| Path::new(home).join(".claude"))
            })
            .ok_or_else(invalid)?;
        if !config.is_absolute() {
            return Err(invalid());
        }
        let config = config.canonicalize()?;
        let roster = small_json(&config.join("daemon/roster.json"), 1024 * 1024, &config)?;
        let row = roster
            .pointer(&format!("/workers/{short}"))
            .ok_or_else(invalid)?;
        let host = pid(&row["pid"])?;
        let repl = pid(&row["replPid"])?;
        let session = small_json(
            &config.join("sessions").join(format!("{repl}.json")),
            262144,
            &config,
        )?;
        let domain = local_domain()?;
        if std::fs::read_link(format!("/proc/{repl}/ns/pid"))?
            != std::fs::read_link("/proc/self/ns/pid")?
            || std::fs::read_link(format!("/proc/{host}/ns/pid"))?
                != std::fs::read_link("/proc/self/ns/pid")?
        {
            return Err(invalid());
        }
        let expected_cwd = cwd.canonicalize()?;
        let mut row = row.clone();
        let row_key = if row["worktreePath"].is_string() {
            "worktreePath"
        } else {
            "cwd"
        };
        row[row_key] = serde_json::json!(Path::new(text(&row, row_key)?).canonicalize()?);
        let mut session = session;
        session["cwd"] = serde_json::json!(Path::new(text(&session, "cwd")?).canonicalize()?);
        parse(
            &row,
            &session,
            short,
            expected_session,
            &expected_cwd,
            &domain,
            &creation(host)?,
            &creation(repl)?,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn values() -> (Value, Value) {
        (
            json!({"pid":41,"procStart":"100","replPid":42,"replProcStart":"200","sessionId":"saved","cwd":"/work","cliVersion":"2.1.293","attempt":2}),
            json!({"pid":42,"procStart":"200","sessionId":"saved","cwd":"/work","version":"2.1.293","kind":"bg","jobId":"abcdef12","pidDomain":"linux:machine:pid:[1]"}),
        )
    }
    #[test]
    fn links_provider_repl_separately_from_host_without_granting_stop_authority() {
        let (row, session) = values();
        let binding = parse(
            &row,
            &session,
            "abcdef12",
            "saved",
            Path::new("/work"),
            "linux:machine:pid:[1]",
            "100",
            "200",
        )
        .expect("matching observation");
        assert_eq!(binding.host_pid, 41);
        assert_eq!(binding.repl_pid, 42);
    }
    #[test]
    fn stale_cross_job_domain_or_unknown_version_is_not_attributed() {
        let (row, session) = values();
        for (field, wrong) in [
            ("pid", json!(41)),
            ("procStart", json!("201")),
            ("sessionId", json!("other")),
            ("jobId", json!("abcdef13")),
            ("cwd", json!("/other")),
            ("version", json!("2.1.294")),
            ("pidDomain", json!("other-domain")),
            ("kind", json!("interactive")),
        ] {
            let mut changed = session.clone();
            changed[field] = wrong;
            assert!(
                parse(
                    &row,
                    &changed,
                    "abcdef12",
                    "saved",
                    Path::new("/work"),
                    "linux:machine:pid:[1]",
                    "100",
                    "200"
                )
                .is_err(),
                "{field}"
            );
        }
        assert!(
            parse(
                &row,
                &session,
                "abcdef12",
                "saved",
                Path::new("/work"),
                "linux:machine:pid:[1]",
                "101",
                "200"
            )
            .is_err()
        );
    }
}
