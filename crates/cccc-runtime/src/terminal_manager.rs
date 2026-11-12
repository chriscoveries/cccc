use crate::RuntimeError;
use crate::registry::{lookup, with_session};
use crate::terminal_attach::{AttachmentRegistry, TerminalAttachMode, TerminalAttachment};

pub fn attach(
    group_id: &str,
    actor_id: &str,
    mode: TerminalAttachMode,
    takeover: bool,
    since: Option<u64>,
) -> Result<TerminalAttachment, RuntimeError> {
    attach_inner(group_id, actor_id, mode, takeover, since, false, None)
}

pub fn attach_with_snapshot(
    group_id: &str,
    actor_id: &str,
    mode: TerminalAttachMode,
    takeover: bool,
    since: Option<u64>,
) -> Result<TerminalAttachment, RuntimeError> {
    attach_inner(group_id, actor_id, mode, takeover, since, true, None)
}

pub fn attach_with_size(
    group_id: &str,
    actor_id: &str,
    mode: TerminalAttachMode,
    takeover: bool,
    since: Option<u64>,
    cols: u16,
    rows: u16,
) -> Result<TerminalAttachment, RuntimeError> {
    attach_inner(
        group_id,
        actor_id,
        mode,
        takeover,
        since,
        false,
        Some((cols, rows)),
    )
}

pub fn attach_with_snapshot_and_size(
    group_id: &str,
    actor_id: &str,
    mode: TerminalAttachMode,
    takeover: bool,
    since: Option<u64>,
    cols: u16,
    rows: u16,
) -> Result<TerminalAttachment, RuntimeError> {
    attach_inner(
        group_id,
        actor_id,
        mode,
        takeover,
        since,
        true,
        Some((cols, rows)),
    )
}

fn attach_inner(
    group_id: &str,
    actor_id: &str,
    mode: TerminalAttachMode,
    takeover: bool,
    since: Option<u64>,
    prefer_snapshot: bool,
    initial_size: Option<(u16, u16)>,
) -> Result<TerminalAttachment, RuntimeError> {
    with_session(group_id, actor_id, |session| {
        session.attach(mode, takeover, since, prefer_snapshot, initial_size)
    })
}

pub fn attachment_writable(
    group_id: &str,
    actor_id: &str,
    attachment_id: u64,
) -> Result<bool, RuntimeError> {
    with_session(group_id, actor_id, |session| {
        session.attachment_writable(attachment_id)
    })
}

pub fn resize_from_attachment(
    group_id: &str,
    actor_id: &str,
    attachment_id: u64,
    cols: u16,
    rows: u16,
) -> Result<bool, RuntimeError> {
    with_session(group_id, actor_id, |session| {
        session.resize_from_attachment(attachment_id, cols, rows)
    })
}

pub(crate) fn write_from_attachment(
    group_id: &str,
    actor_id: &str,
    registry: &AttachmentRegistry,
    attachment_id: u64,
    data: &[u8],
) -> Result<bool, RuntimeError> {
    if data.is_empty() {
        return Ok(true);
    }
    let session = lookup(group_id, actor_id)?;
    let gate = session
        .lock()
        .map_err(|_| RuntimeError::Poisoned)?
        .input_gate();
    // Revocation is authoritative here too: a dropped or replaced attachment
    // must release the input lane even if its Actor has stopped reading.
    let revoked = || !matches!(registry.is_writer(attachment_id), Ok(true));
    let Some(_guard) = crate::cancellation::lock_interruptibly(&gate, &revoked)? else {
        return Ok(false);
    };
    let writer = session
        .lock()
        .map_err(|_| RuntimeError::Poisoned)?
        .input_writer_from_attachment(registry, attachment_id)?;
    let Some(writer) = writer else {
        return Ok(false);
    };
    crate::pty_input::write_input_interruptibly(&writer, data, &revoked)
}

pub(crate) fn control_from_attachment(
    group_id: &str,
    actor_id: &str,
    registry: &AttachmentRegistry,
    attachment_id: u64,
    operation: impl FnOnce() -> Result<(), RuntimeError>,
) -> Result<bool, RuntimeError> {
    let busy = || {
        RuntimeError::Io(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "terminal session registry is busy",
        ))
    };
    // Pin the current terminal incarnation through bounded local admission.
    // Every lock is a try-lock; no provider I/O may occur in operation.
    let sessions = crate::registry::sessions().try_read().map_err(|_| busy())?;
    let session = sessions
        .get(&(group_id.to_owned(), actor_id.to_owned()))
        .ok_or_else(|| RuntimeError::NotFound(group_id.into(), actor_id.into()))?;
    let session = session.try_lock().map_err(|_| busy())?;
    session.control_from_attachment(registry, attachment_id, operation)
}
