use super::DiskEvent;
use cccc_core::HomeLayout;
use fs2::FileExt;
use serde_json::{Value, json};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;

fn path(home: &HomeLayout) -> PathBuf {
    home.daemon_dir().join("disk-events.jsonl")
}

fn visit(home: &HomeLayout, mut receive: impl FnMut(DiskEvent) -> bool) -> io::Result<()> {
    let file = match File::open(path(home)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    FileExt::lock_shared(&file)?;
    let mut reader = BufReader::new(file);
    loop {
        let mut bytes = Vec::new();
        let read = (&mut reader)
            .take(64 * 1024)
            .read_until(b'\n', &mut bytes)?;
        if read == 0 {
            break;
        }
        if bytes.last() != Some(&b'\n') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "incomplete or oversized disk event",
            ));
        }
        let event: DiskEvent = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        if event.v != 1 || event.kind != "disk.threshold_crossed" || event.scope != "cccc_home" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported disk event",
            ));
        }
        if !receive(event) {
            break;
        }
    }
    Ok(())
}

pub(super) fn latest(home: &HomeLayout) -> io::Result<Option<DiskEvent>> {
    let mut latest = None;
    visit(home, |event| {
        latest = Some(event);
        true
    })?;
    Ok(latest)
}

pub(super) fn append(home: &HomeLayout, event: &DiskEvent) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(event).map_err(io::Error::other)?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(path(home))?;
    FileExt::lock_exclusive(&file)?;
    let original_len = file.metadata()?.len();
    if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_data()) {
        file.set_len(original_len)?;
        file.sync_data()?;
        return Err(error);
    }
    #[cfg(unix)]
    if original_len == 0 {
        // Persist the newly created journal name as well as its first record.
        File::open(home.daemon_dir())?.sync_all()?;
    }
    Ok(())
}

pub(crate) fn read_events(
    home: &HomeLayout,
    after: Option<&str>,
    limit: usize,
) -> io::Result<Value> {
    let mut found = after.is_none();
    let mut events = Vec::with_capacity(limit + 1);
    visit(home, |event| {
        if after == Some(event.id.as_str()) {
            found = true;
            events.clear();
        } else if events.len() <= limit {
            events.push(event);
        }
        // Before finding a cursor, keep only a bounded first page while scanning.
        !found || events.len() <= limit
    })?;
    let has_more = events.len() > limit;
    events.truncate(limit);
    let cursor = events
        .last()
        .map(|event| event.id.as_str())
        .or(if found { after } else { None });
    Ok(json!({"events":events,"cursor":cursor,"has_more":has_more,"gap":!found}))
}
