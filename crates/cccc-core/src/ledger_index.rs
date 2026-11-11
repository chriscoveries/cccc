use cccc_contracts::Event;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Arc;

use crate::ledger::snapshot::{Location, Snapshot, Source};

mod cache;
mod queries;
pub(crate) use queries::{
    events_after, find_event, find_idempotent, find_relay, inspect, inspect_status, read_all,
};

type ClientKey = (String, String, String);

// No parsed Event or JSON body survives a query. These are source locations
// and the small identity fields needed to preserve generated legacy defaults.
#[derive(Clone)]
struct Record {
    location: Location,
    id: String,
    ts: String,
}

#[derive(Clone, Default)]
struct LedgerIndex {
    sources: Vec<Source>,
    offsets: Vec<u64>,
    records: Vec<Record>,
    positions: HashMap<String, usize>,
    client_ids: HashMap<ClientKey, usize>,
    relays: HashMap<String, usize>,
    actor_generations: HashMap<String, usize>,
    active_prefix: Option<[u8; 32]>,
    line_aligned: bool,
}

impl LedgerIndex {
    fn push(&mut self, event: Event, location: Location) {
        let position = self.records.len();
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
        self.records.push(Record {
            location,
            id: event.id,
            ts: event.ts,
        });
        // event.data, including oversized actor.activity bodies, drops here.
    }

    fn events(
        &self,
        snapshot: &Snapshot,
        positions: impl IntoIterator<Item = usize>,
    ) -> io::Result<Vec<Event>> {
        let records = positions
            .into_iter()
            .map(|p| &self.records[p])
            .collect::<Vec<_>>();
        let locations = records.iter().map(|r| r.location).collect::<Vec<_>>();
        let mut events = snapshot.events(&locations)?;
        for (event, record) in events.iter_mut().zip(records) {
            // Event permits absent id/ts in legacy records. Reuse the defaults
            // generated at indexing, rather than generating new ones on a read.
            event.id.clone_from(&record.id);
            event.ts.clone_from(&record.ts);
        }
        Ok(events)
    }

    fn weight(&self) -> u64 {
        // Account owned capacities conservatively, including lookup tables.
        // This bounds accounted retained metadata, not allocator RSS or the
        // temporary Vec<Event> explicitly requested by the bulk inspect API.
        let strings = self
            .records
            .iter()
            .map(|r| r.id.capacity() + r.ts.capacity())
            .sum::<usize>()
            + self.positions.keys().map(String::capacity).sum::<usize>()
            + self.relays.keys().map(String::capacity).sum::<usize>()
            + self
                .actor_generations
                .keys()
                .map(String::capacity)
                .sum::<usize>()
            + self
                .client_ids
                .keys()
                .map(|(a, b, c)| a.capacity() + b.capacity() + c.capacity())
                .sum::<usize>();
        let tables = (self.positions.capacity()
            + self.relays.capacity()
            + self.actor_generations.capacity())
            * std::mem::size_of::<(String, usize)>()
            + self.client_ids.capacity() * std::mem::size_of::<(ClientKey, usize)>();
        let sources = self
            .sources
            .iter()
            .map(|s| s.path.as_os_str().len())
            .sum::<usize>()
            + self.sources.capacity() * std::mem::size_of::<Source>();
        ((std::mem::size_of::<Self>()
            + strings
            + tables
            + sources
            + self.offsets.capacity() * std::mem::size_of::<u64>()
            + self.records.capacity() * std::mem::size_of::<Record>()) as u64)
            .saturating_mul(2)
    }
}

fn index_actor_generation(
    event: &Event,
    position: usize,
    generations: &mut HashMap<String, usize>,
) {
    if event.kind == "actor.add"
        && let Some(id) = event
            .data
            .get("actor")
            .and_then(serde_json::Value::as_object)
            .and_then(|actor| actor.get("id"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
    {
        generations.insert(id.to_owned(), position);
    }
}

struct IndexedSnapshot {
    entry: Arc<cache::CachedIndex>,
    snapshot: Snapshot,
}

fn current(path: &Path) -> io::Result<IndexedSnapshot> {
    // The stable lock spans capture, continuity proof and metadata catch-up.
    // No cache lock is held during source I/O or decoding.
    let _lock = crate::ledger::acquire_reader_lock(path)?;
    let snapshot = Snapshot::open(path)?;
    let previous = cache::get(path);
    if let Some(entry) = previous.as_ref()
        && entry.index.sources == snapshot.sources
    {
        return Ok(IndexedSnapshot {
            entry: Arc::clone(entry),
            snapshot,
        });
    }
    let resumes = if let Some(entry) = previous.as_ref() {
        let old = &entry.index;
        old.line_aligned
            && snapshot.extends(&old.sources)
            && match snapshot.active_index() {
                Some(active) => {
                    old.active_prefix
                        == Some(prefix_hash(&snapshot, active, old.sources[active].len)?)
                }
                None => false,
            }
    } else {
        false
    };
    let mut index = if resumes {
        previous.as_ref().expect("previous").index.clone()
    } else {
        LedgerIndex::default()
    };
    #[cfg(test)]
    BUILD_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
    let previous_offsets = index.offsets.clone();
    let offsets = snapshot.visit(&previous_offsets, |event, location| {
        index.push(event, location);
        ControlFlow::Continue(())
    })?;
    index.offsets = offsets;
    index.active_prefix = snapshot
        .active_index()
        .map(|active| prefix_hash(&snapshot, active, snapshot.sources[active].len))
        .transpose()?;
    index.line_aligned = snapshot.ends_with_newline()?;
    index.sources.clone_from(&snapshot.sources);
    drop(previous);
    let entry = cache::admit(path, index);
    Ok(IndexedSnapshot { entry, snapshot })
}

fn prefix_hash(snapshot: &Snapshot, source: usize, len: u64) -> io::Result<[u8; 32]> {
    let mut file = snapshot.source_file(source)?;
    file.seek(SeekFrom::Start(0))?;
    let mut reader = file.take(len);
    let mut hash = Sha256::new();
    let mut bytes = [0_u8; 64 * 1024];
    loop {
        let count = reader.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        hash.update(&bytes[..count]);
    }
    if reader.limit() != 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(hash.finalize().into())
}

// Queries reconcile canonical committed bytes, independently of notification
// timing. A delayed/duplicate notification cannot replay an Event into the index.
pub(crate) fn note_append(_path: &Path, _event: &Event, _encoded_len: usize) {}

pub(crate) fn invalidate_path(path: &Path) {
    cache::invalidate(path);
}

#[cfg(test)]
pub(crate) fn is_cached(path: &Path) -> bool {
    cache::get(path).is_some()
}

#[cfg(test)]
thread_local! {
    static BUILD_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::Duration;

    fn captured(kind: &str) -> Event {
        include_str!("../tests/fixtures/ledger-index-memory/sample.jsonl")
            .lines()
            .map(|line| serde_json::from_str::<Event>(line).expect("captured event"))
            .find(|event| event.kind == kind)
            .expect("captured kind")
    }
    fn pair() -> (Event, Event) {
        (captured("chat.message"), captured("actor.activity"))
    }
    fn commit_without_notification(path: &Path, event: &Event) {
        let _lock = crate::ledger::acquire_writer_lock(path).expect("writer lock");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .expect("source");
        serde_json::to_writer(&mut file, event).expect("encode");
        file.write_all(b"\n").expect("newline");
        file.sync_data().expect("commit");
    }
    fn all(view: &IndexedSnapshot) -> Vec<Event> {
        view.entry
            .index
            .events(&view.snapshot, 0..view.entry.index.records.len())
            .expect("hydrate")
    }

    #[test]
    fn delayed_notification_cannot_apply_an_already_indexed_event_for_a_new_append() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("ledger.jsonl");
        let (first, second) = pair();
        crate::ledger::append(&path, &first).expect("first");
        assert_eq!(
            crate::ledger::read_all(&path).expect("cold"),
            vec![first.clone()]
        );
        commit_without_notification(&path, &second);
        note_append(
            &path,
            &first,
            serde_json::to_vec(&first).expect("encode").len() + 1,
        );
        assert_eq!(
            crate::ledger::read_all(&path).expect("current"),
            vec![first, second.clone()]
        );
        note_append(
            &path,
            &second,
            serde_json::to_vec(&second).expect("encode").len() + 1,
        );
        assert_eq!(
            cache::get(&path).expect("cache").index.sources,
            Snapshot::open(&path).expect("snapshot").sources
        );
    }

    #[test]
    fn a_warm_query_waits_for_a_rotation_in_progress() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("ledger.jsonl");
        let first = captured("chat.message");
        crate::ledger::append(&path, &first).expect("first");
        crate::ledger::read_all(&path).expect("warm");
        let rotation = crate::ledger::acquire_writer_lock(&path).expect("writer lock");
        let reader_path = path.clone();
        let reader = std::thread::spawn(move || crate::ledger::read_all(&reader_path));
        std::thread::sleep(Duration::from_millis(100));
        assert!(!reader.is_finished(), "reader must wait for rotation");
        drop(rotation);
        assert_eq!(reader.join().expect("reader").expect("read"), vec![first]);
    }

    #[test]
    fn a_stale_warm_index_catches_up_with_an_unnotified_commit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("ledger.jsonl");
        let (first, second) = pair();
        crate::ledger::append(&path, &first).expect("first");
        let previous = current(&path).expect("warm");
        commit_without_notification(&path, &second);
        let before = crate::ledger::snapshot::decoded_records();
        let next = current(&path).expect("caught up");
        assert_eq!(
            crate::ledger::snapshot::decoded_records() - before,
            1,
            "old JSON must not be reparsed"
        );
        assert_eq!(
            all(&previous),
            vec![first.clone()],
            "borrowed snapshots stay immutable"
        );
        assert_eq!(all(&next), vec![first, second]);
    }

    #[test]
    fn concurrent_cold_read_and_append_do_not_duplicate_the_committed_event() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("ledger.jsonl");
        let (first, second) = pair();
        crate::ledger::append(&path, &first).expect("first");
        let (entered, waiting) = std::sync::mpsc::channel();
        let (release, resume) = std::sync::mpsc::channel();
        let reader_path = path.clone();
        let reader = std::thread::spawn(move || {
            BUILD_HOOK.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    entered.send(()).expect("entered");
                    resume
                        .recv_timeout(Duration::from_secs(5))
                        .expect("release");
                }))
            });
            crate::ledger::read_all(&reader_path)
        });
        waiting
            .recv_timeout(Duration::from_secs(5))
            .expect("source-read barrier");
        let writer_path = path.clone();
        let writer_event = second.clone();
        let writer = std::thread::spawn(move || crate::ledger::append(&writer_path, &writer_event));
        std::thread::sleep(Duration::from_millis(50));
        assert!(!writer.is_finished(), "stable snapshot lock blocks append");
        release.send(()).expect("resume");
        assert_eq!(
            reader.join().expect("reader").expect("read"),
            vec![first.clone()]
        );
        writer.join().expect("writer").expect("append");
        assert_eq!(
            crate::ledger::read_all(&path).expect("current"),
            vec![first, second]
        );
    }

    #[test]
    fn rewrite_with_growth_rebuilds_instead_of_using_old_locations() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("ledger.jsonl");
        let (first, second) = pair();
        crate::ledger::append(&path, &first).expect("first");
        let old = current(&path).expect("warm");
        drop(old);
        // Labeled mutation: replace the indexed id and append a captured row.
        let mut changed = first.clone();
        changed.id = "0".repeat(first.id.len());
        let mut bytes = serde_json::to_vec(&changed).expect("encode");
        bytes.push(b'\n');
        bytes.extend(serde_json::to_vec(&second).expect("encode"));
        bytes.push(b'\n');
        std::fs::write(&path, bytes).expect("rewrite and grow");
        assert_eq!(
            all(&current(&path).expect("rebuilt")),
            vec![changed, second]
        );
        assert!(find_event(&path, &first.id).expect("old id").is_none());
    }

    #[test]
    fn replacement_truncation_and_partial_lines_reset_safely() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("ledger.jsonl");
        let (first, second) = pair();
        crate::ledger::append(&path, &first).expect("first");
        let original = current(&path).expect("warm");
        let new_path = temp.path().join("replacement");
        let mut bytes = serde_json::to_vec(&second).expect("encode");
        bytes.push(b'\n');
        std::fs::write(&new_path, &bytes).expect("replacement");
        // Rename the old active file, as compaction does, including on Windows.
        std::fs::rename(&path, temp.path().join("old")).expect("old active");
        std::fs::rename(new_path, &path).expect("replace");
        assert_eq!(
            all(&current(&path).expect("replacement")),
            vec![second.clone()]
        );
        assert_eq!(all(&original), vec![first.clone()]);
        std::fs::write(&path, []).expect("truncate");
        assert!(all(&current(&path).expect("truncated")).is_empty());
        bytes.truncate(bytes.len() / 2);
        std::fs::write(&path, bytes).expect("partial captured row");
        assert!(all(&current(&path).expect("partial")).is_empty());
        std::fs::write(&path, serde_json::to_vec(&second).expect("encode"))
            .expect("complete without newline");
        assert_eq!(all(&current(&path).expect("complete")), vec![second]);
    }

    #[test]
    fn cold_activity_body_is_reparsed_only_on_demand_and_gzip_preserves_order() {
        use flate2::{Compression, write::GzEncoder};
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("ledger.jsonl");
        let (first, activity) = pair();
        let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
        serde_json::to_writer(&mut gzip, &first).expect("gzip row");
        gzip.write_all(b"\n").expect("newline");
        let archives = temp.path().join("state/ledger/segments");
        std::fs::create_dir_all(&archives).expect("archive dir");
        std::fs::write(
            archives.join("ledger.000001.jsonl.gz"),
            gzip.finish().expect("finish"),
        )
        .expect("archive");
        crate::ledger::append(&path, &activity).expect("activity");
        let view = current(&path).expect("index");
        assert_eq!(view.entry.index.records.len(), 2);
        assert_eq!(
            find_event(&path, &activity.id).expect("activity"),
            Some(activity.clone())
        );
        assert_eq!(all(&view), vec![first, activity]);
    }
    #[test]
    fn parsed_payload_retention_is_zero_and_bounded_pages_load_only_requested_bodies() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("ledger.jsonl");
        let (first, activity) = pair();
        crate::ledger::append(&path, &first).expect("first");
        crate::ledger::append(&path, &activity).expect("activity");
        let before = crate::ledger::snapshot::hydrated_records();
        let _view = current(&path).expect("metadata only");
        assert_eq!(crate::ledger::snapshot::hydrated_records(), before);
        assert_eq!(
            find_event(&path, &activity.id).expect("body"),
            Some(activity.clone())
        );
        assert_eq!(crate::ledger::snapshot::hydrated_records() - before, 1);
        assert_eq!(
            events_after(&path, &first.id, 1).expect("page"),
            vec![activity.clone()]
        );
        assert_eq!(crate::ledger::snapshot::hydrated_records() - before, 2);
        let location = Location {
            source: 0,
            offset: 0,
            end: 0,
        };
        let mut with_body = LedgerIndex::default();
        with_body.push(activity.clone(), location);
        let mut without_body = LedgerIndex::default();
        let mut stripped = activity;
        stripped.data.clear(); // Labeled mutation: remove the captured body.
        without_body.push(stripped, location);
        assert_eq!(
            with_body.weight(),
            without_body.weight(),
            "body bytes are never retained"
        );
    }

    #[test]
    fn legacy_defaults_stay_stable_and_failed_rebuilds_do_not_replace_a_good_index() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("ledger.jsonl");
        let event = captured("chat.message");
        let mut legacy = serde_json::to_value(event).expect("captured value");
        // Labeled mutation of a captured record: exercise serde defaults.
        legacy.as_object_mut().expect("object").remove("id");
        legacy.as_object_mut().expect("object").remove("ts");
        std::fs::write(&path, format!("{legacy}\n")).expect("legacy row");
        let events = read_all(&path).expect("defaults");
        assert_eq!(
            find_event(&path, &events[0].id).expect("stable defaults"),
            Some(events[0].clone())
        );
        let before = cache::get(&path).expect("cached");
        let archives = temp.path().join("state/ledger/segments");
        std::fs::create_dir_all(&archives).expect("segments");
        std::fs::write(archives.join("ledger.000001.jsonl.gz"), b"not gzip")
            .expect("corrupt archive");
        assert!(current(&path).is_err());
        assert!(Arc::ptr_eq(
            &before,
            &cache::get(&path).expect("unchanged cache")
        ));
    }
    #[test]
    fn warm_lookup_and_page_wait_for_a_file_locked_partial_append() {
        use fs2::FileExt;
        use std::sync::mpsc::{self, RecvTimeoutError};
        for page in [false, true] {
            let temp = tempfile::tempdir().expect("tempdir");
            let path = temp.path().join("ledger.jsonl");
            let (first, second) = pair();
            crate::ledger::append(&path, &first).expect("captured first row");
            crate::ledger::read_all(&path).expect("warm index");
            let encoded = serde_json::to_vec(&second).expect("captured second row");
            let split = encoded.len() / 2;
            let mut writer = std::fs::OpenOptions::new()
                .append(true)
                .read(true)
                .open(&path)
                .expect("active file");
            writer.lock_exclusive().expect("legacy file lock");
            writer.write_all(&encoded[..split]).expect("partial row");
            writer.flush().expect("flush partial row");
            let (started, start) = mpsc::sync_channel(1);
            let (send, receive) = mpsc::sync_channel(1);
            let reader_path = path.clone();
            let first_id = first.id.clone();
            let second_id = second.id.clone();
            let reader = std::thread::spawn(move || {
                started.send(()).expect("reader started");
                let result = if page {
                    crate::ledger::events_after(&reader_path, &first_id, 1)
                } else {
                    crate::ledger::find_event(&reader_path, &second_id)
                        .map(|event| event.into_iter().collect::<Vec<_>>())
                };
                send.send(result).expect("send result");
            });
            start
                .recv_timeout(Duration::from_secs(2))
                .expect("reader started");
            let early = receive.recv_timeout(Duration::from_millis(50));
            let blocked = matches!(&early, Err(RecvTimeoutError::Timeout));
            // Complete/unlock even if the negative control failed, then join.
            writer.write_all(&encoded[split..]).expect("complete row");
            writer.write_all(b"\n").expect("newline");
            writer.flush().expect("flush complete row");
            FileExt::unlock(&writer).expect("unlock writer");
            let result = match early {
                Ok(result) => result,
                Err(RecvTimeoutError::Timeout) => receive
                    .recv_timeout(Duration::from_secs(2))
                    .expect("completed read"),
                Err(error) => panic!("reader disconnected: {error}"),
            };
            reader.join().expect("join reader");
            assert!(
                blocked,
                "warm query observed an in-progress file-locked append"
            );
            assert_eq!(result.expect("query"), vec![second]);
        }
    }
}
