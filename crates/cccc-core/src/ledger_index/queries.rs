use cccc_contracts::Event;
use fs2::FileExt;
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::path::Path;

use super::current;

fn source_locks(path: &Path) -> io::Result<(File, Option<File>)> {
    // Preserve both writer contracts: stable lock for append/rotation, then
    // the active-file lock used by existing file-locked writers. Acquire both
    // before capturing lengths, and retain them through lazy body hydration.
    let stable = crate::ledger::acquire_reader_lock(path)?;
    let active = match File::open(path) {
        Ok(file) => {
            FileExt::lock_shared(&file)?;
            Some(file)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    Ok((stable, active))
}

pub(crate) fn inspect<T>(
    path: &Path,
    inspect: impl FnOnce(&[Event], &HashMap<String, usize>) -> T,
) -> io::Result<T> {
    let source_lock = source_locks(path)?;
    let view = current(path)?;
    let index = &view.entry.index;
    let events = index.events(&view.snapshot, 0..index.records.len())?;
    drop(source_lock);
    Ok(inspect(&events, &index.positions))
}

pub(crate) fn inspect_status<T>(
    path: &Path,
    inspect: impl FnOnce(&[Event], &HashMap<String, usize>, &HashMap<String, usize>) -> T,
) -> io::Result<T> {
    let source_lock = source_locks(path)?;
    let view = current(path)?;
    let index = &view.entry.index;
    let events = index.events(&view.snapshot, 0..index.records.len())?;
    drop(source_lock);
    Ok(inspect(&events, &index.positions, &index.actor_generations))
}

fn find(
    path: &Path,
    position: impl FnOnce(&super::LedgerIndex) -> Option<usize>,
) -> io::Result<Option<Event>> {
    let _source_lock = source_locks(path)?;
    let view = current(path)?;
    let Some(position) = position(&view.entry.index) else {
        return Ok(None);
    };
    Ok(view.entry.index.events(&view.snapshot, [position])?.pop())
}

pub(crate) fn find_event(path: &Path, event_id: &str) -> io::Result<Option<Event>> {
    find(path, |index| index.positions.get(event_id).copied())
}

pub(crate) fn find_idempotent(
    path: &Path,
    kind: &str,
    by: &str,
    client_id: &str,
) -> io::Result<Option<Event>> {
    let key = (kind.to_owned(), by.to_owned(), client_id.to_owned());
    find(path, |index| index.client_ids.get(&key).copied())
}

pub(crate) fn find_relay(path: &Path, source_event_id: &str) -> io::Result<Option<Event>> {
    find(path, |index| index.relays.get(source_event_id).copied())
}

pub(crate) fn events_after(path: &Path, event_id: &str, limit: usize) -> io::Result<Vec<Event>> {
    let _source_lock = source_locks(path)?;
    let view = current(path)?;
    let index = &view.entry.index;
    let Some(start) = index.positions.get(event_id).copied() else {
        return Ok(Vec::new());
    };
    let start = start.saturating_add(1);
    index.events(
        &view.snapshot,
        start..start.saturating_add(limit).min(index.records.len()),
    )
}

pub(crate) fn read_all(path: &Path) -> io::Result<Vec<Event>> {
    let _source_lock = source_locks(path)?;
    let view = current(path)?;
    view.entry
        .index
        .events(&view.snapshot, 0..view.entry.index.records.len())
}
