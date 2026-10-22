//! Which Actor a terminal's process says it is.
//!
//! A runtime session is keyed by group and Actor, and its process starts with
//! that `CCCC_GROUP_ID` and `CCCC_ACTOR_ID`. Nothing keeps the process from
//! later running as someone else (a wrapper or runtime can exec under another
//! identity), and input typed into it is then acted on as that other Actor. So
//! before an Actor's terminal writer is handed out, the process is asked again.

use crate::RuntimeError;

/// Refuses unless the process still reports the group and Actor its session
/// was started for. Fails closed: a process that reports another identity, no
/// identity, or whose environment cannot be read (another user's or a
/// non-dumpable process, or one already torn down) gets no input.
#[cfg(target_os = "linux")]
pub(crate) fn verify(pid: Option<u32>, group_id: &str, actor_id: &str) -> Result<(), RuntimeError> {
    let found = match pid.map(|pid| std::fs::read(format!("/proc/{pid}/environ"))) {
        None => "no process id".to_owned(),
        Some(Err(error)) => format!("an unreadable environment ({error})"),
        Some(Ok(environ)) => match (
            value(&environ, b"CCCC_GROUP_ID"),
            value(&environ, b"CCCC_ACTOR_ID"),
        ) {
            (Some(group), Some(actor))
                if group == group_id.as_bytes() && actor == actor_id.as_bytes() =>
            {
                return Ok(());
            }
            (Some(group), Some(actor)) => format!(
                "{}/{}",
                String::from_utf8_lossy(group),
                String::from_utf8_lossy(actor)
            ),
            _ if environ.is_empty() => "an empty environment".to_owned(),
            _ => "no Actor identity".to_owned(),
        },
    };
    Err(RuntimeError::IdentityMismatch {
        group_id: group_id.to_owned(),
        actor_id: actor_id.to_owned(),
        found,
    })
}

/// Other platforms expose no other process's environment this way, so input
/// is not checked there.
#[cfg(not(target_os = "linux"))]
pub(crate) fn verify(
    _pid: Option<u32>,
    _group_id: &str,
    _actor_id: &str,
) -> Result<(), RuntimeError> {
    Ok(())
}

/// The first `key=value` entry of a NUL-separated environment, as getenv
/// would see it.
#[cfg(target_os = "linux")]
fn value<'a>(environ: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    environ
        .split(|byte| *byte == 0)
        .find_map(|entry| entry.strip_prefix(key)?.strip_prefix(b"="))
}
