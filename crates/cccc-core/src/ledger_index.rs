use cccc_contracts::Event;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

use crate::ledger::{
    EventLocation, SourceRevision, is_gzip, ledger_group_id, read_source_from_indexed, revisions,
    visit_indexed_events,
};

mod cache;
mod queries;
pub(crate) use queries::{find_event, find_idempotent, find_relay, inspect, inspect_status};

type ClientKey = (String, String, String);
const MAX_HOT_EVENT_BYTES: u64 = 32 * 1024 * 1024;
const MAX_COLD_POINT_EVENT_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Default)]
struct ColdPointCache {
    events: HashMap<usize, Event>,
    order: VecDeque<usize>,
    estimated_bytes: u64,
}

impl ColdPointCache {
    fn get(&self, position: usize) -> Option<Event> {
        self.events.get(&position).cloned()
    }

    fn insert(&mut self, position: usize, event: Event) {
        if self.events.contains_key(&position) {
            return;
        }
        let bytes = estimate_event_bytes(&event);
        if bytes > MAX_COLD_POINT_EVENT_BYTES {
            return;
        }
        self.estimated_bytes = self.estimated_bytes.saturating_add(bytes);
        self.order.push_back(position);
        self.events.insert(position, event);
        while self.estimated_bytes > MAX_COLD_POINT_EVENT_BYTES {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(event) = self.events.remove(&oldest) {
                self.estimated_bytes = self
                    .estimated_bytes
                    .saturating_sub(estimate_event_bytes(&event));
            }
        }
    }
}

#[derive(Default)]
struct LedgerIndex {
    revisions: Vec<SourceRevision>,
    /// The newest parsed events. Older records are addressed through `locations`.
    events: VecDeque<Event>,
    events_start: usize,
    locations: Vec<EventLocation>,
    positions: HashMap<String, usize>,
    client_ids: HashMap<ClientKey, usize>,
    relays: HashMap<String, usize>,
    replied_by: HashMap<String, BTreeSet<String>>,
    estimated_bytes: u64,
    /// Recently requested cold point lookups. These need not be contiguous
    /// with the newest hot window, so they have a separate bounded cache.
    cold_points: Mutex<ColdPointCache>,
}

impl LedgerIndex {
    fn rebuild(path: &Path, revisions: Vec<SourceRevision>) -> io::Result<Self> {
        let mut index = Self {
            revisions,
            ..Self::default()
        };
        let sources = index.revisions.clone();
        visit_indexed_events(path, &sources, |event, location| {
            index.push_indexed(event, location);
        })?;
        Ok(index)
    }

    fn push(&mut self, event: Event, location: EventLocation, next_revisions: Vec<SourceRevision>) {
        self.push_indexed(event, location);
        self.revisions = next_revisions;
    }

    fn push_indexed(&mut self, event: Event, location: EventLocation) {
        let position = self.locations.len();
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
        self.locations.push(location);
        self.events.push_back(event);
        self.trim_hot_events(MAX_HOT_EVENT_BYTES);
    }

    /// Apply the events committed since the indexed revisions without
    /// re-reading the sources that produced the retained events.
    fn catch_up(
        &mut self,
        events: Vec<(Event, EventLocation)>,
        next_revisions: Vec<SourceRevision>,
    ) {
        for (event, location) in events {
            self.push_indexed(event, location);
        }
        self.revisions = next_revisions;
    }

    fn trim_hot_events(&mut self, max_bytes: u64) {
        while self.estimated_bytes > max_bytes {
            let Some(event) = self.events.pop_front() else {
                break;
            };
            self.estimated_bytes = self
                .estimated_bytes
                .saturating_sub(estimate_event_bytes(&event));
            self.events_start = self.events_start.saturating_add(1);
        }
        if self.events.is_empty() {
            self.events.shrink_to_fit();
        }
    }
}

/// The events a stale index can adopt by reading only appended bytes: every
/// previously indexed source is still present and line-aligned, so the tail
/// after the old length extends the history the index already holds. Anything
/// else (a removed, truncated, rewritten, or gzip-grown source, or a torn old
/// tail line) is unreadable as an incremental delta and the caller falls back
/// to a full rebuild.
fn read_appended_tail(
    path: &Path,
    old: &[SourceRevision],
    next: &[SourceRevision],
) -> io::Result<Option<Vec<(Event, EventLocation)>>> {
    if old.is_empty() {
        // A stale-marked or cold index holds no committed prefix to extend.
        return Ok(None);
    }
    let group_id = ledger_group_id(path);
    let mut remaining = old
        .iter()
        .map(|revision| (revision.path.clone(), revision.len))
        .collect::<HashMap<_, _>>();
    let mut events = Vec::new();
    for (source_index, revision) in next.iter().enumerate() {
        match remaining.remove(&revision.path) {
            Some(previous_len) if previous_len > revision.len => return Ok(None),
            Some(previous_len) if is_gzip(&revision.path) && previous_len != revision.len => {
                // Gzip sources cannot be resumed mid-stream.
                return Ok(None);
            }
            Some(previous_len) => {
                if previous_len > 0 && !old_tail_is_line_aligned(&revision.path, previous_len)? {
                    return Ok(None);
                }
                if revision.len > previous_len {
                    let (mut read, _) = read_source_from_indexed(
                        &revision.path,
                        previous_len,
                        &group_id,
                        source_index,
                    )?;
                    events.append(&mut read);
                }
            }
            None => {
                let (mut read, _) =
                    read_source_from_indexed(&revision.path, 0, &group_id, source_index)?;
                events.append(&mut read);
            }
        }
    }
    if !remaining.is_empty() {
        return Ok(None);
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

pub(crate) fn note_append(path: &Path, event: &Event, encoded_len: usize, record_offset: u64) {
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
    let source_index = next_revisions
        .iter()
        .position(|revision| revision.path == path);
    // A delayed callback may describe an event already loaded by a rebuild,
    // while the file has grown by a different event of the same encoded size.
    if exact_append
        && other_sources_unchanged
        && !index.positions.contains_key(&event.id)
        && let (Some(next_len), Some(source_index)) = (next_len, source_index)
    {
        let location = EventLocation {
            source_index,
            offset: record_offset,
            len: next_len.saturating_sub(record_offset),
        };
        index.push(event.clone(), location, next_revisions);
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
        let record_offset = std::fs::metadata(&path).expect("first source").len();
        let source_lock = crate::ledger::acquire_writer_lock(&path).expect("writer lock");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("source");
        file.write_all(&encoded).expect("second event");
        file.sync_data().expect("commit");
        drop(source_lock);
        note_append(&path, &first, encoded.len(), record_offset);
        assert_eq!(
            crate::ledger::read_all(&path).expect("current index"),
            vec![first, second.clone()]
        );
        note_append(&path, &second, encoded.len(), record_offset);
        let entry = cache::get(&path).expect("cache");
        assert_eq!(
            entry.read().expect("index").revisions,
            revisions(&path).expect("revisions")
        );
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
        assert_eq!(
            index.events.iter().cloned().collect::<Vec<_>>(),
            vec![first, second]
        );
        assert_eq!(index.revisions, revisions(&path).expect("revisions"));
    }

    #[test]
    fn cold_event_lookup_and_full_inspection_reparse_evicted_events() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("ledger.jsonl");
        let mut expected = Vec::new();
        for number in 0..4 {
            let mut event = Event::new("chat.message", "g_fixture");
            event.id = format!("event-{number}");
            event.data.insert(
                "text".into(),
                serde_json::json!(format!("message {number}")),
            );
            crate::ledger::append(&path, &event).expect("append fixture");
            expected.push(event);
        }

        let entry = current(&path).expect("build index");
        {
            let mut index = entry.write().expect("index write");
            index.trim_hot_events(0);
            assert!(index.events.is_empty());
            assert_eq!(index.events_start, expected.len());
            assert_eq!(index.positions.len(), expected.len());
            assert_eq!(index.locations.len(), expected.len());
        }

        for event in &expected {
            assert_eq!(
                crate::ledger::find_event(&path, &event.id).expect("cold lookup"),
                Some(event.clone())
            );
        }
        let index = entry.read().expect("index read");
        let cold_points = index.cold_points.lock().expect("cold point cache");
        assert_eq!(cold_points.events.len(), expected.len());
        assert!(cold_points.estimated_bytes <= MAX_COLD_POINT_EVENT_BYTES);
        drop(cold_points);
        drop(index);
        let inspected = crate::ledger::inspect(&path, |events, _| {
            events
                .iter()
                .map(|event| event.id.clone())
                .collect::<Vec<_>>()
        })
        .expect("full history inspection");
        assert_eq!(
            inspected,
            expected
                .iter()
                .map(|event| event.id.clone())
                .collect::<Vec<_>>()
        );

        let appended = Event::new("chat.message", "g_fixture");
        crate::ledger::append(&path, &appended).expect("append after eviction");
        assert_eq!(
            crate::ledger::find_event(&path, &appended.id).expect("hot append lookup"),
            Some(appended)
        );
        assert_eq!(
            crate::ledger::find_event(&path, &expected[0].id).expect("old event lookup"),
            Some(expected[0].clone())
        );
    }

    #[test]
    fn cold_event_lookup_reads_compressed_segments() {
        use std::io::Write;

        let temp = tempfile::tempdir().expect("tempdir");
        let group_dir = temp.path().join("g_fixture");
        let path = group_dir.join("ledger.jsonl");
        let segments = group_dir.join("state/ledger/segments");
        std::fs::create_dir_all(&segments).expect("segments");
        let archived = Event::new("chat.message", "g_fixture");
        let encoded = format!("{}\n", serde_json::to_string(&archived).expect("serialize"));
        let mut gzip = flate2::write::GzEncoder::new(
            std::fs::File::create(segments.join("0001.jsonl.gz")).expect("gzip file"),
            flate2::Compression::default(),
        );
        gzip.write_all(encoded.as_bytes()).expect("write segment");
        gzip.finish().expect("finish gzip");
        crate::ledger::append(&path, &Event::new("actor.add", "g_fixture"))
            .expect("append active event");

        let entry = current(&path).expect("build index");
        entry.write().expect("index write").trim_hot_events(0);
        assert_eq!(
            crate::ledger::find_event(&path, &archived.id).expect("compressed cold lookup"),
            Some(archived)
        );
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
