use super::*;
use flate2::{Compression, write::GzEncoder};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

fn capture() -> Vec<Event> {
    serde_json::from_str(include_str!(
        "../../tests/fixtures/mail_notice_read_history.json"
    ))
    .expect("real captured Mail history")
}

fn encode(events: &[Event]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for event in events {
        serde_json::to_writer(&mut bytes, event).expect("event");
        bytes.push(b'\n');
    }
    bytes
}

fn append_without_callback(path: &Path, bytes: &[u8]) {
    let _lock = crate::ledger::acquire_writer_lock(path).expect("writer");
    let mut file = OpenOptions::new().append(true).open(path).expect("file");
    file.write_all(bytes).expect("append");
    file.sync_data().expect("commit");
}

fn assert_parity(path: &Path) {
    assert_eq!(
        crate::ledger::read_all(path).expect("indexed"),
        read_all_uncached(path).expect("uncached")
    );
}

fn segment(path: &Path, name: &str, events: &[Event]) -> PathBuf {
    let dir = path.parent().expect("parent").join("state/ledger/segments");
    fs::create_dir_all(&dir).expect("segments");
    let file = dir.join(name);
    if name.ends_with(".gz") {
        let mut gzip = GzEncoder::new(
            std::fs::File::create(&file).expect("archive"),
            Compression::fast(),
        );
        gzip.write_all(&encode(events)).expect("compressed events");
        gzip.finish().expect("finish");
    } else {
        fs::write(&file, encode(events)).expect("segment");
    }
    file
}

#[test]
fn a_missed_callback_decodes_only_the_captured_active_tail() {
    let dir = tempfile::tempdir().expect("directory");
    let path = dir.path().join("ledger.jsonl");
    let events = capture();
    fs::write(&path, encode(&events[..events.len() - 1])).expect("prefix");
    let cold_start = crate::ledger::snapshot::decoded_records();
    assert_eq!(
        crate::ledger::read_all(&path).expect("cold"),
        events[..events.len() - 1]
    );
    let cold = crate::ledger::snapshot::decoded_records() - cold_start;
    append_without_callback(&path, &encode(&events[events.len() - 1..]));
    let warm_start = crate::ledger::snapshot::decoded_records();
    assert_eq!(crate::ledger::read_all(&path).expect("warm"), events);
    let warm = crate::ledger::snapshot::decoded_records() - warm_start;
    assert_eq!(
        warm, 1,
        "warm catch-up must decode the tail, not the captured prefix"
    );
    assert!(cold > warm);
    assert_parity(&path);
}

#[test]
fn immutable_compressed_history_is_not_decoded_again_for_active_growth() {
    let dir = tempfile::tempdir().expect("directory");
    let path = dir.path().join("ledger.jsonl");
    let events = capture();
    segment(&path, "ledger.000001.jsonl.gz", &events[..events.len() - 2]);
    fs::write(&path, encode(&events[events.len() - 2..events.len() - 1])).expect("active");
    assert_parity(&path);
    append_without_callback(&path, &encode(&events[events.len() - 1..]));
    let before = crate::ledger::snapshot::decoded_records();
    assert_eq!(crate::ledger::read_all(&path).expect("warm"), events);
    assert_eq!(crate::ledger::snapshot::decoded_records() - before, 1);
    assert_parity(&path);
}

#[test]
fn replacement_and_truncation_rebuild_even_for_delayed_append_hints() {
    let dir = tempfile::tempdir().expect("directory");
    let path = dir.path().join("ledger.jsonl");
    let events = capture();
    fs::write(&path, encode(&events[..2])).expect("prefix");
    assert_parity(&path);
    let mtime = fs::metadata(&path)
        .expect("metadata")
        .modified()
        .expect("mtime");
    let mut reordered = events[..2].to_vec();
    reordered.reverse();
    crate::fs::atomic_write(&path, &encode(&reordered)).expect("replace same-size file");
    OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("file")
        .set_modified(mtime)
        .expect("preserve mtime");
    assert_eq!(
        crate::ledger::read_all(&path).expect("replacement"),
        reordered
    );
    crate::fs::atomic_write(&path, &encode(&events[2..])).expect("different growing replacement");
    note_append(&path, &events[0], encode(&events[..1]).len());
    assert_eq!(
        crate::ledger::read_all(&path).expect("callback must read actual replacement"),
        events[2..]
    );
    fs::write(&path, encode(&events[..1])).expect("truncate");
    assert_eq!(
        crate::ledger::read_all(&path).expect("truncation"),
        events[..1]
    );
    assert_parity(&path);
}

#[test]
fn torn_or_unaligned_prefixes_rebuild_instead_of_skipping_bytes() {
    let dir = tempfile::tempdir().expect("directory");
    let path = dir.path().join("ledger.jsonl");
    let events = capture();
    let tail = encode(&events[1..2]);
    let mut initial = encode(&events[..1]);
    initial.extend_from_slice(&tail[..tail.len() / 2]);
    fs::write(&path, initial).expect("interrupted write control");
    assert_eq!(
        crate::ledger::read_all(&path).expect("partial"),
        events[..1]
    );
    append_without_callback(&path, &tail[tail.len() / 2..]);
    assert_eq!(
        crate::ledger::read_all(&path).expect("completed"),
        events[..2]
    );
    let mut unaligned = encode(&events[..1]);
    unaligned.pop();
    crate::fs::atomic_write(&path, &unaligned).expect("no newline control");
    assert_eq!(
        crate::ledger::read_all(&path).expect("valid JSON without newline"),
        events[..1]
    );
    let mut growth = vec![b'\n'];
    growth.extend_from_slice(&tail);
    append_without_callback(&path, &growth);
    assert_eq!(
        crate::ledger::read_all(&path).expect("aligned again"),
        events[..2]
    );
    assert_parity(&path);
}

#[test]
fn source_add_remove_reorder_rotation_and_gzip_growth_rebuild() {
    let dir = tempfile::tempdir().expect("directory");
    let path = dir.path().join("ledger.jsonl");
    let events = capture();
    fs::write(&path, encode(&events[2..])).expect("active");
    assert_parity(&path);
    let archive = segment(&path, "ledger.000002.jsonl.gz", &events[1..2]);
    assert_parity(&path);
    let earlier = segment(&path, "ledger.000001.jsonl", &events[..1]);
    assert_eq!(
        crate::ledger::read_all(&path).expect("ordered sources"),
        events
    );
    fs::rename(&earlier, earlier.with_file_name("ledger.000003.jsonl")).expect("reorder");
    assert_parity(&path);
    segment(&path, "ledger.000002.jsonl.gz", &events[..2]);
    assert_parity(&path);
    fs::remove_file(&archive).expect("remove source");
    assert_parity(&path);
    let rotated = segment(&path, "ledger.000004.jsonl", &events[2..]);
    fs::write(&path, encode(&events[..1])).expect("new active");
    assert_parity(&path);
    fs::remove_file(rotated).expect("remove rotated");
    assert_parity(&path);
    let entry = cache::get(&path).expect("cache");
    entry.write().expect("index").sources.clear();
    append_without_callback(&path, &encode(&events[1..2]));
    assert_parity(&path);
}
