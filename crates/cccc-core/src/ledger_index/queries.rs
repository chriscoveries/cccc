use cccc_contracts::Event;
use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::Path;

use super::current;

pub(crate) fn inspect<T>(
    path: &Path,
    inspect: impl FnOnce(&[Event], &HashMap<String, usize>) -> T,
) -> io::Result<T> {
    loop {
        let entry = current(path)?;
        let index = entry
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if index.events_start == 0 && index.events.len() == index.locations.len() {
            let (first, second) = index.events.as_slices();
            if second.is_empty() {
                return Ok(inspect(first, &index.positions));
            }
            let events = index.events.iter().cloned().collect::<Vec<_>>();
            return Ok(inspect(&events, &index.positions));
        }
        match crate::ledger::read_all_if_current(path, &index.revisions)? {
            Some(events) => return Ok(inspect(&events, &index.positions)),
            None => continue,
        }
    }
}

pub(crate) fn inspect_status<T>(
    path: &Path,
    inspect: impl FnOnce(&[Event], &HashMap<String, usize>, &HashMap<String, BTreeSet<String>>) -> T,
) -> io::Result<T> {
    loop {
        let entry = current(path)?;
        let index = entry
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if index.events_start == 0 && index.events.len() == index.locations.len() {
            let (first, second) = index.events.as_slices();
            if second.is_empty() {
                return Ok(inspect(first, &index.positions, &index.replied_by));
            }
            let events = index.events.iter().cloned().collect::<Vec<_>>();
            return Ok(inspect(&events, &index.positions, &index.replied_by));
        }
        match crate::ledger::read_all_if_current(path, &index.revisions)? {
            Some(events) => {
                return Ok(inspect(&events, &index.positions, &index.replied_by));
            }
            None => continue,
        }
    }
}

pub(crate) fn find_event(path: &Path, event_id: &str) -> io::Result<Option<Event>> {
    loop {
        let entry = current(path)?;
        let index = entry
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(position) = index.positions.get(event_id).copied() else {
            return Ok(None);
        };
        match event_at(path, &index, position)? {
            Some(event) => return Ok(Some(event)),
            None => continue,
        }
    }
}

pub(crate) fn find_idempotent(
    path: &Path,
    kind: &str,
    by: &str,
    client_id: &str,
) -> io::Result<Option<Event>> {
    let key = (kind.to_owned(), by.to_owned(), client_id.to_owned());
    loop {
        let entry = current(path)?;
        let index = entry
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(position) = index.client_ids.get(&key).copied() else {
            return Ok(None);
        };
        match event_at(path, &index, position)? {
            Some(event) => return Ok(Some(event)),
            None => continue,
        }
    }
}

pub(crate) fn find_relay(path: &Path, source_event_id: &str) -> io::Result<Option<Event>> {
    loop {
        let entry = current(path)?;
        let index = entry
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(position) = index.relays.get(source_event_id).copied() else {
            return Ok(None);
        };
        match event_at(path, &index, position)? {
            Some(event) => return Ok(Some(event)),
            None => continue,
        }
    }
}

fn event_at(path: &Path, index: &super::LedgerIndex, position: usize) -> io::Result<Option<Event>> {
    if position >= index.events_start {
        return Ok(index.events.get(position - index.events_start).cloned());
    }
    if let Some(event) = index
        .cold_points
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(position)
    {
        return Ok(Some(event));
    }
    let location = index
        .locations
        .get(position)
        .copied()
        .ok_or_else(|| io::Error::other("ledger event location is missing"))?;
    let event = crate::ledger::read_event_at(path, &index.revisions, location)?;
    if let Some(event) = event.as_ref() {
        index
            .cold_points
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(position, event.clone());
    }
    Ok(event)
}
