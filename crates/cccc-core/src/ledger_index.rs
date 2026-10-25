use cccc_contracts::Event;
use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::{Arc, RwLock};

use crate::ledger::snapshot::{Snapshot, Source};
use crate::ledger::{SourceRevision, read_all_uncached, revisions};
use std::ops::ControlFlow;

mod cache;
mod queries;
pub(crate) use queries::{find_event, find_idempotent, find_relay, inspect, inspect_status};

type ClientKey = (String, String, String);

#[derive(Default)]
struct LedgerIndex {
    revisions: Vec<SourceRevision>,
    sources: Vec<Source>,
    offsets: Vec<u64>,
    events: Vec<Event>,
    positions: HashMap<String, usize>,
    client_ids: HashMap<ClientKey, usize>,
    relays: HashMap<String, usize>,
    actor_generations: HashMap<String, usize>,
    estimated_bytes: u64,
}

impl LedgerIndex {
    fn rebuild(
        path: &Path,
        revisions: Vec<SourceRevision>,
        snapshot: Option<&Snapshot>,
    ) -> io::Result<Self> {
        let mut index = Self {
            revisions,
            ..Self::default()
        };
        if let Some(snapshot) = snapshot {
            index.offsets = snapshot.visit(&[], |event, _| {
                index.events.push(event);
                ControlFlow::Continue(())
            })?;
            index.sources = snapshot.sources.clone();
        } else {
            // Identity uncertainty disables reuse rather than hiding otherwise readable records.
            index.events = read_all_uncached(path)?;
        }
        index.reindex();
        index.estimated_bytes = estimate_events_bytes(&index.events);
        Ok(index)
    }

    fn refresh(
        &mut self,
        path: &Path,
        revisions: Vec<SourceRevision>,
        snapshot: Option<&Snapshot>,
    ) -> io::Result<()> {
        if let Some(snapshot) = snapshot
            && let Some(offsets) = snapshot.append_offsets(&self.sources, &self.offsets)?
        {
            let mut tail = Vec::new();
            let ends = snapshot.visit(&offsets, |event, _| {
                tail.push(event);
                ControlFlow::Continue(())
            })?;
            if snapshot.fully_consumed(&ends)? {
                for event in tail {
                    self.push(event);
                }
                self.sources = snapshot.sources.clone();
                self.offsets = ends;
                self.revisions = revisions;
                return Ok(());
            }
        }
        *self = Self::rebuild(path, revisions, snapshot)?;
        Ok(())
    }

    fn reindex(&mut self) {
        self.positions.clear();
        self.client_ids.clear();
        self.relays.clear();
        self.actor_generations.clear();
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
            index_actor_generation(event, position, &mut self.actor_generations);
        }
    }

    fn push(&mut self, event: Event) {
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
        index_actor_generation(&event, position, &mut self.actor_generations);
        self.estimated_bytes = self
            .estimated_bytes
            .saturating_add(estimate_event_bytes(&event));
        self.events.push(event);
    }
}

fn index_actor_generation(
    event: &Event,
    position: usize,
    actor_generations: &mut HashMap<String, usize>,
) {
    if event.kind != "actor.add" {
        return;
    }
    if let Some(actor_id) = event
        .data
        .get("actor")
        .and_then(serde_json::Value::as_object)
        .and_then(|actor| actor.get("id"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|actor_id| !actor_id.is_empty())
    {
        actor_generations.insert(actor_id.to_owned(), position);
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
    // Observe under the ledger lock, then release it before taking the index
    // lock. Appenders also release their writer lock before notifying the index.
    let (observed, sources) = {
        let _source_lock = crate::ledger::acquire_reader_lock(path)?;
        (
            revisions(path)?,
            Snapshot::open_locked(path)
                .ok()
                .map(|snapshot| snapshot.sources),
        )
    };
    let weight = observed.iter().map(|revision| revision.len).sum();
    let entry = cache::entry(path, weight);
    {
        let index = entry
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if index.revisions == observed
            && sources
                .as_ref()
                .is_some_and(|sources| *sources == index.sources)
        {
            return Ok(Arc::clone(&entry));
        }
    }
    let mut index = entry
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let source_lock = crate::ledger::acquire_reader_lock(path)?;
    let next_revisions = revisions(path)?;
    let snapshot = Snapshot::open_locked(path).ok();
    if index.revisions != next_revisions
        || snapshot
            .as_ref()
            .is_none_or(|snapshot| snapshot.sources != index.sources)
    {
        index.refresh(path, next_revisions, snapshot.as_ref())?;
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

pub(crate) fn note_append(path: &Path, _event: &Event, _encoded_len: usize) {
    let Some(cached) = cache::get(path) else {
        return;
    };
    let mut index = cached
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Callbacks are hints, not authority for the bytes or file generation.
    // Use the same ordered-source/identity/consumed-offset validation as queries.
    let result = (|| {
        let _source_lock = crate::ledger::acquire_reader_lock(path)?;
        let next = revisions(path)?;
        let snapshot = Snapshot::open_locked(path).ok();
        if index.revisions != next
            || snapshot
                .as_ref()
                .is_none_or(|snapshot| snapshot.sources != index.sources)
        {
            index.refresh(path, next, snapshot.as_ref())?;
        }
        Ok::<_, io::Error>(
            index
                .revisions
                .iter()
                .map(|revision| revision.len)
                .sum::<u64>()
                .max(index.estimated_bytes),
        )
    })();
    match result {
        Ok(weight) => {
            drop(index);
            cache::update_weight(path, weight, &cached);
        }
        Err(_) => {
            index.revisions.clear();
            index.sources.clear();
            index.offsets.clear();
        }
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
#[path = "ledger_index/tail_tests.rs"]
mod tail_tests;

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
