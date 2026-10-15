use cccc_contracts::Event;
use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::Path;
use std::sync::{Arc, RwLock};

use crate::ledger::{
    SourceRevision, is_gzip, ledger_group_id, read_all_uncached, read_source_from, revisions,
};

mod cache;
mod queries;
pub(crate) use queries::{find_event, find_idempotent, find_relay, inspect, inspect_status};

type ClientKey = (String, String, String);

#[derive(Default)]
struct LedgerIndex {
    revisions: Vec<SourceRevision>,
    events: Vec<Event>,
    positions: HashMap<String, usize>,
    client_ids: HashMap<ClientKey, usize>,
    relays: HashMap<String, usize>,
    replied_by: HashMap<String, BTreeSet<String>>,
    estimated_bytes: u64,
}

impl LedgerIndex {
    fn rebuild(path: &Path, revisions: Vec<SourceRevision>) -> io::Result<Self> {
        let events = read_all_uncached(path)?;
        let mut index = Self {
            revisions,
            events,
            ..Self::default()
        };
        index.reindex();
        index.estimated_bytes = estimate_events_bytes(&index.events);
        Ok(index)
    }

    fn reindex(&mut self) {
        self.positions.clear();
        self.client_ids.clear();
        self.relays.clear();
        self.replied_by.clear();
        for (position, event) in self.events.iter().enumerate() {
            self.positions.insert(event.id.clone(), position);
            if let Some(client_id) = event
                .data
                .get("client_id")
                .and_then(serde_json::Value::as_str)
            {
                self.client_ids.insert(
                    (event.kind.clone(), event.by.clone(), client_id.to_owned()),
                    position,
                );
            }
            if event.kind == "chat.message"
                && let Some(source_id) = event
                    .data
                    .get("src_event_id")
                    .and_then(serde_json::Value::as_str)
            {
                self.relays.insert(source_id.to_owned(), position);
            }
            index_reply(event, &mut self.replied_by);
        }
    }

    fn push(&mut self, event: Event, next_revisions: Vec<SourceRevision>) {
        self.push_indexed(event);
        self.revisions = next_revisions;
    }

    fn push_indexed(&mut self, event: Event) {
        let position = self.events.len();
        self.positions.insert(event.id.clone(), position);
        if let Some(client_id) = event
            .data
            .get("client_id")
            .and_then(serde_json::Value::as_str)
        {
            self.client_ids.insert(
                (event.kind.clone(), event.by.clone(), client_id.to_owned()),
                position,
            );
        }
        if event.kind == "chat.message"
            && let Some(source_id) = event
                .data
                .get("src_event_id")
                .and_then(serde_json::Value::as_str)
        {
            self.relays.insert(source_id.to_owned(), position);
        }
        index_reply(&event, &mut self.replied_by);
        self.estimated_bytes = self
            .estimated_bytes
            .saturating_add(estimate_event_bytes(&event));
        self.events.push(event);
    }

    /// Apply the events committed since the indexed revisions without
    /// re-reading the sources that produced the retained events.
    fn catch_up(&mut self, events: Vec<Event>, next_revisions: Vec<SourceRevision>) {
        for event in events {
            self.push_indexed(event);
        }
        self.revisions = next_revisions;
    }
}

/// Extend a retained prefix only across ordinary appends to the active file.
/// Rotation can reuse that pathname at an equal or greater length, so every
/// ordered source path and every sealed revision must remain unchanged.
fn read_appended_tail(
    path: &Path,
    old: &[SourceRevision],
    next: &[SourceRevision],
) -> io::Result<Option<Vec<Event>>> {
    if old.is_empty()
        || old.len() != next.len()
        || old
            .iter()
            .zip(next)
            .any(|(old, next)| old.path != next.path)
    {
        return Ok(None);
    }
    let group_id = ledger_group_id(path);
    let mut events = Vec::new();
    for (previous, revision) in old.iter().zip(next) {
        if revision.path != path {
            // Historical sources are sealed. Appending their new bytes after
            // the retained active events would also reorder canonical history.
            if previous != revision {
                return Ok(None);
            }
            continue;
        }
        if previous.len > revision.len {
            return Ok(None);
        }
        if previous.len == revision.len {
            // Same-size changed metadata cannot certify an appended tail.
            if previous != revision {
                return Ok(None);
            }
            continue;
        }
        if is_gzip(&revision.path)
            || (previous.len > 0 && !old_tail_is_line_aligned(&revision.path, previous.len)?)
        {
            return Ok(None);
        }
        let (mut read, consumed_end) = read_source_from(&revision.path, previous.len, &group_id)?;
        if consumed_end != revision.len {
            // Never certify bytes that were not consumed as the indexed tail.
            return Ok(None);
        }
        events.append(&mut read);
    }
    Ok(Some(events))
}

/// The retained events end on a line boundary only when the byte before the
/// old length is a newline; a torn old tail means the first bytes after it
/// continue a line the index never parsed.
fn old_tail_is_line_aligned(path: &Path, previous_len: u64) -> io::Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(previous_len - 1))?;
    let mut last = [0_u8; 1];
    file.read_exact(&mut last)?;
    Ok(last[0] == b'\n')
}

fn index_reply(event: &Event, replied_by: &mut HashMap<String, BTreeSet<String>>) {
    if event.kind != "chat.message" {
        return;
    }
    let target = event
        .data
        .get("reply_to")
        .and_then(serde_json::Value::as_str);
    let actor = Some(event.by.as_str());
    if let (Some(target), Some(actor)) = (target, actor)
        && !target.is_empty()
        && !actor.is_empty()
    {
        replied_by
            .entry(target.to_owned())
            .or_default()
            .insert(actor.to_owned());
    }
}

fn estimate_events_bytes(events: &[Event]) -> u64 {
    events.iter().map(estimate_event_bytes).sum()
}

fn estimate_event_bytes(event: &Event) -> u64 {
    let strings = [
        &event.id,
        &event.ts,
        &event.kind,
        &event.group_id,
        &event.scope_key,
        &event.by,
    ]
    .into_iter()
    .map(|value| value.capacity() as u64)
    .sum::<u64>();
    // IDs and relation keys are duplicated by the lookup maps. A factor of
    // two keeps the cache budget conservative without a second allocation.
    (std::mem::size_of::<Event>() as u64)
        .saturating_add(strings)
        .saturating_add(estimate_map_bytes(&event.data))
        .saturating_mul(2)
}

fn estimate_map_bytes(map: &serde_json::Map<String, serde_json::Value>) -> u64 {
    map.iter()
        .map(|(key, value)| key.capacity() as u64 + estimate_value_bytes(value))
        .sum::<u64>()
        .saturating_add((map.len() * std::mem::size_of::<(String, serde_json::Value)>()) as u64)
}

fn estimate_value_bytes(value: &serde_json::Value) -> u64 {
    match value {
        serde_json::Value::String(value) => value.capacity() as u64,
        serde_json::Value::Array(values) => values
            .iter()
            .map(estimate_value_bytes)
            .sum::<u64>()
            .saturating_add((values.capacity() * std::mem::size_of::<serde_json::Value>()) as u64),
        serde_json::Value::Object(map) => estimate_map_bytes(map),
        _ => std::mem::size_of::<serde_json::Value>() as u64,
    }
}

fn current(path: &Path) -> io::Result<Arc<RwLock<LedgerIndex>>> {
    // Observe the sources under the stable ledger lock, then release it before
    // taking the index lock below so the lock order stays index -> ledger.
    // Compaction renames the active file and adds segments; outside the lock a
    // look can miss a source, and a partial list can even match a cached one
    // (path, length and mtime are not a file generation), returning stale events.
    let observed = {
        let _source_lock = crate::ledger::acquire_reader_lock(path)?;
        revisions(path)?
    };
    let weight = observed.iter().map(|revision| revision.len).sum();
    let entry = cache::entry(path, weight);
    if entry
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .revisions
        == observed
    {
        return Ok(entry);
    }
    let mut index = entry
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Metadata and events must describe the same committed history. Refresh
    // after both locks: another reader or append may have updated the index
    // while this reader waited. Appenders release the writer lock before
    // note_append takes the index lock, so this order cannot form a lock cycle.
    let source_lock = crate::ledger::acquire_reader_lock(path)?;
    let next_revisions = revisions(path)?;
    if index.revisions != next_revisions {
        match read_appended_tail(path, &index.revisions, &next_revisions) {
            Ok(Some(tail_events)) => index.catch_up(tail_events, next_revisions),
            _ => *index = LedgerIndex::rebuild(path, next_revisions)?,
        }
    }
    let weight = index
        .revisions
        .iter()
        .map(|revision| revision.len)
        .sum::<u64>()
        .max(index.estimated_bytes);
    drop(source_lock);
    drop(index);
    cache::update_weight(path, weight, &entry);
    Ok(entry)
}

pub(crate) fn note_append(path: &Path, event: &Event, encoded_len: usize) {
    let cached = cache::get(path);
    let Some(cached) = cached else { return };
    let Ok(next_revisions) = revisions(path) else {
        return;
    };
    let source_bytes: u64 = next_revisions.iter().map(|revision| revision.len).sum();
    let mut index = cached
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if index.revisions == next_revisions {
        // A rebuild already included this committed append.
        return;
    }
    let previous_len = index
        .revisions
        .iter()
        .find(|revision| revision.path == path)
        .map(|revision| revision.len);
    let next_len = next_revisions
        .iter()
        .find(|revision| revision.path == path)
        .map(|revision| revision.len);
    let other_sources_unchanged = index
        .revisions
        .iter()
        .filter(|revision| revision.path != path)
        .eq(next_revisions
            .iter()
            .filter(|revision| revision.path != path));
    let exact_append = previous_len
        .zip(next_len)
        .is_some_and(|(before, after)| after == before.saturating_add(encoded_len as u64));
    // A delayed callback may describe an event already loaded by a rebuild,
    // while the file has grown by a different event of the same encoded size.
    if exact_append && other_sources_unchanged && !index.positions.contains_key(&event.id) {
        index.push(event.clone(), next_revisions);
        let weight = source_bytes.max(index.estimated_bytes);
        drop(index);
        cache::update_weight(path, weight, &cached);
    } else {
        index.revisions.clear();
    }
}

pub(crate) fn invalidate_path(path: &Path) {
    cache::invalidate(path);
}

#[cfg(test)]
pub(crate) fn is_cached(path: &Path) -> bool {
    cache::get(path).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use fs2::FileExt;
    #[cfg(unix)]
    use std::fs::File;
    #[cfg(unix)]
    use std::thread;
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    #[test]
    fn delayed_notification_cannot_apply_an_already_indexed_event_for_a_new_append() {
        use std::io::Write;

        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("ledger.jsonl");
        let first = Event::new("chat.message", "g_fixture");
        let mut second = first.clone();
        second.id = "x".repeat(first.id.len());
        crate::ledger::append(&path, &first).expect("first event");
        assert_eq!(
            crate::ledger::read_all(&path).expect("cold index"),
            vec![first.clone()]
        );

        // Model the interval after a second writer commits and before its
        // callback. The first writer's callback may arrive during this interval.
        let mut encoded = serde_json::to_vec(&second).expect("encode");
        encoded.push(b'\n');
        let source_lock = crate::ledger::acquire_writer_lock(&path).expect("writer lock");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("source");
        file.write_all(&encoded).expect("second event");
        file.sync_data().expect("commit");
        drop(source_lock);
        note_append(&path, &first, encoded.len());
        assert_eq!(
            crate::ledger::read_all(&path).expect("current index"),
            vec![first, second.clone()]
        );
        note_append(&path, &second, encoded.len());
        let entry = cache::get(&path).expect("cache");
        assert_eq!(
            entry.read().expect("index").revisions,
            revisions(&path).expect("revisions")
        );
    }

    fn assert_rotation_refill_before_notification(refill_count: usize) {
        use std::io::Write;

        let temp = tempfile::tempdir().expect("isolated rotation fixture");
        let home = crate::HomeLayout::from_path(temp.path()).expect("home");
        let store = crate::GroupStore::new(home.clone()).expect("store");
        let group = store.create("rotation refill", "").expect("group");
        let path = store.ledger_path(&group.group_id).expect("ledger");
        // Any creation records remain canonical but do not change the exact
        // active-file byte boundary used by this regression.
        crate::ledger_archive::compact(&home, &group.group_id, "seed boundary")
            .expect("initial boundary");
        let mut first = Event::new("chat.message", &group.group_id);
        first.id = "a".repeat(32);
        crate::ledger::append(&path, &first).expect("first append");
        let mut expected = crate::ledger::read_all(&path).expect("warm index");
        let first_len = std::fs::metadata(&path).expect("active source").len();
        crate::ledger_archive::compact(&home, &group.group_id, "rotate warm active")
            .expect("rotate")
            .expect("segment");

        let mut pending_notifications = Vec::new();
        for ordinal in 0..refill_count {
            let mut event = first.clone();
            event.id = char::from(b'b' + ordinal as u8).to_string().repeat(32);
            let mut encoded = serde_json::to_vec(&event).expect("encode");
            encoded.push(b'\n');
            assert_eq!(encoded.len() as u64, first_len, "equal-size native records");
            let writer = crate::ledger::acquire_writer_lock(&path).expect("writer lock");
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("active source");
            file.write_all(&encoded).expect("refill commit");
            file.sync_data().expect("durable refill");
            drop(file);
            drop(writer);
            // This is the production interval after append releases its writer
            // lock and before note_append acquires the index lock.
            expected.push(event.clone());
            pending_notifications.push((event, encoded.len()));
        }
        assert_eq!(
            crate::ledger::read_all(&path).expect("query before notifications"),
            expected,
            "rotation plus refill must preserve the exact canonical sequence",
        );
        for (event, encoded_len) in pending_notifications {
            note_append(&path, &event, encoded_len);
        }
        assert_eq!(
            crate::ledger::read_all(&path).expect("query after notifications"),
            expected,
            "delayed notifications must neither duplicate nor skip committed IDs",
        );
    }

    #[test]
    fn rotation_refilled_to_equal_length_before_notification_keeps_the_exact_sequence() {
        assert_rotation_refill_before_notification(1);
    }

    #[test]
    fn rotation_refilled_past_old_length_before_notifications_keeps_the_exact_sequence() {
        assert_rotation_refill_before_notification(2);
    }

    #[test]
    fn a_warm_query_waits_for_a_rotation_in_progress() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("ledger.jsonl");
        let first = Event::new("chat.message", "g_fixture");
        crate::ledger::append(&path, &first).expect("first event");
        assert_eq!(
            crate::ledger::read_all(&path).expect("warm"),
            vec![first.clone()]
        );

        // Compaction holds the writer lock while it moves sources. Even a warm
        // index must not compare against a source list taken mid-rotation.
        let rotation = crate::ledger::acquire_writer_lock(&path).expect("writer lock");
        let reader_path = path.clone();
        let reader = std::thread::spawn(move || crate::ledger::read_all(&reader_path));
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            !reader.is_finished(),
            "a warm query must not observe sources while a rotation holds the ledger"
        );
        drop(rotation);
        assert_eq!(reader.join().expect("reader").expect("read"), vec![first]);
    }

    #[test]
    fn a_stale_warm_index_catches_up_from_the_appended_tail_only() {
        use std::io::Write;

        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("ledger.jsonl");
        let first = Event::new("chat.message", "g_fixture");
        crate::ledger::append(&path, &first).expect("first event");
        assert_eq!(
            crate::ledger::read_all(&path).expect("warm index"),
            vec![first.clone()]
        );

        // Rewrite the committed prefix in place, then append a second event.
        // A full rebuild would fail on the corrupted first line; the tail
        // catch-up keeps the already-indexed prefix and reads only the tail.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("rewrite");
        file.write_all(b"x").expect("corrupt first byte");
        file.sync_data().expect("commit corruption");
        drop(file);
        let second = Event::new("chat.message", "g_fixture");
        let mut encoded = serde_json::to_vec(&second).expect("encode");
        encoded.push(b'\n');
        let writer = crate::ledger::acquire_writer_lock(&path).expect("writer lock");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("append");
        file.write_all(&encoded).expect("second event");
        file.sync_data().expect("commit");
        drop(writer);

        assert_eq!(
            crate::ledger::read_all(&path).expect("caught up"),
            vec![first, second]
        );
    }

    #[test]
    fn a_stale_warm_index_catches_up_with_an_unnotified_commit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("ledger.jsonl");
        let first = Event::new("chat.message", "g_fixture");
        let second = Event::new("chat.message", "g_fixture");
        crate::ledger::append(&path, &first).expect("first event");
        let entry = current(&path).expect("warm index");

        // Commit without the append notification so the cache really is stale.
        let mut encoded = serde_json::to_vec(&second).expect("encode");
        encoded.push(b'\n');
        let writer = crate::ledger::acquire_writer_lock(&path).expect("writer lock");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("source");
        std::io::Write::write_all(&mut file, &encoded).expect("second event");
        file.sync_data().expect("commit");
        drop(writer);
        assert_eq!(entry.read().expect("index").events.len(), 1);

        let index = current(&path).expect("caught up");
        let index = index.read().expect("index");
        assert_eq!(index.events, vec![first, second]);
        assert_eq!(index.revisions, revisions(&path).expect("revisions"));
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_cold_read_and_append_do_not_duplicate_the_committed_event() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("ledger.jsonl");
        let first = Event::new("chat.message", "g_fixture");
        let second = Event::new("chat.message", "g_fixture");
        crate::ledger::append(&path, &first).expect("first event");

        // Pause the cold reader after revision capture, before source reading.
        // Unix source-file locks are advisory; the actual writer uses its own
        // stable ledger lock, so the old implementation permits an append here.
        let source = File::open(&path).expect("source");
        source.lock_exclusive().expect("pause source reads");
        let reader_path = path.clone();
        let reader = thread::spawn(move || crate::ledger::read_all(&reader_path));
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut rebuilding = false;
        while Instant::now() < deadline {
            if cache::get(&path).is_some_and(|entry| entry.try_read().is_err()) {
                rebuilding = true;
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        let original_len = source.metadata().expect("metadata").len();
        let writer_path = path.clone();
        let writer_event = second.clone();
        let writer = thread::spawn(move || crate::ledger::append(&writer_path, &writer_event));
        let deadline = Instant::now() + Duration::from_millis(200);
        while Instant::now() < deadline
            && source.metadata().expect("metadata").len() == original_len
        {
            thread::sleep(Duration::from_millis(1));
        }
        FileExt::unlock(&source).expect("release reader");
        reader.join().expect("reader thread").expect("read");
        writer.join().expect("writer thread").expect("append");
        assert!(rebuilding, "reader must reach the source-read barrier");
        assert_eq!(
            crate::ledger::read_all(&path).expect("cached events"),
            vec![first, second],
            "a delayed append notification must not duplicate data read during rebuild"
        );
    }
}
