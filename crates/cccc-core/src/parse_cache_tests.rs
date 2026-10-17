//! Correctness tests for the identity-gated parse cache.
//!
//! The speedup is worthless if a reader can ever observe a stale document, so
//! these tests are the point of the change rather than an afterthought.

use super::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Doc {
    title: String,
    count: u32,
}

fn temp() -> (std::sync::MutexGuard<'static, ()>, tempfile::TempDir) {
    // These tests deliberately exercise the process-wide clear operation.
    // Serialize them so one test cannot clear another test's warmed entry.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let guard = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear();
    (guard, tempfile::tempdir().expect("tempdir"))
}

fn write_doc(path: &Path, title: &str, count: u32) {
    crate::fs::write_yaml(
        path,
        &Doc {
            title: title.into(),
            count,
        },
    )
    .expect("write");
}

/// Documents the one case this cache deliberately does not cover, so the
/// limitation is a pinned decision rather than a latent surprise. See the
/// module's `Contract` section.
///
/// A same-length in-place rewrite inside a single filesystem mtime tick keeps
/// all four identity fields identical and is therefore invisible. That is the
/// price of not reading the file, and it is why the entry point is named for
/// atomic writers only.
#[cfg(unix)]
#[test]
fn same_length_in_place_edit_with_preserved_timestamp_is_the_documented_gap() {
    let (_serial, temp) = temp();
    let path = temp.path().join("d.yaml");
    write_doc(&path, "a", 1);
    let first = read_yaml_atomic_cached::<Doc>(&path).expect("first");
    // Other GroupStore tests share this cache. Keep this tiny fixture newer
    // than their entries until the next read, so eviction cannot turn the
    // intended identity-hit assertion into an unrelated cache miss.
    cache()
        .lock()
        .expect("cache")
        .get_mut(&path)
        .expect("entry")
        .last_used = Instant::now() + Duration::from_secs(60);

    // Force an identity collision instead of hoping both writes land in the
    // filesystem's same mtime tick. This writer deliberately violates the
    // atomic-write contract; the unchanged identity can reuse the old parse.
    let metadata = std::fs::metadata(&path).expect("meta");
    let before = FileIdentity::of(&metadata);
    std::fs::write(&path, "title: b\ncount: 2\n").expect("in-place edit");
    std::fs::File::open(&path)
        .expect("file")
        .set_times(std::fs::FileTimes::new().set_modified(metadata.modified().expect("mtime")))
        .expect("preserve mtime");
    let after = FileIdentity::of(&std::fs::metadata(&path).expect("meta"));
    assert_eq!(
        before, after,
        "the fixture must preserve every identity field"
    );
    let second = read_yaml_atomic_cached::<Doc>(&path).expect("second");
    assert_eq!(
        second, first,
        "an unchanged identity reuses the retained parse"
    );
}

/// The guarantee that does hold: any rename-based write is always seen, even
/// when the new content happens to be the same length.
#[test]
fn an_atomic_write_is_always_observed_even_at_equal_length() {
    let (_serial, temp) = temp();
    let path = temp.path().join("d.yaml");
    write_doc(&path, "a", 1);
    assert_eq!(
        read_yaml_atomic_cached::<Doc>(&path).expect("first").title,
        "a"
    );

    write_doc(&path, "b", 2); // "title: b\ncount: 2" — same byte length as "a"/1
    assert_eq!(
        read_yaml_atomic_cached::<Doc>(&path).expect("after").title,
        "b"
    );
}

#[test]
fn caller_receives_an_independent_copy() {
    let (_serial, temp) = temp();
    let path = temp.path().join("d.yaml");
    write_doc(&path, "a", 1);

    let mut first = read_yaml_atomic_cached::<Doc>(&path).expect("first");
    first.count = 99;
    let second = read_yaml_atomic_cached::<Doc>(&path).expect("second");
    assert_eq!(
        second.count, 1,
        "mutating a caller's copy must not affect the cache"
    );
}

#[test]
fn read_after_write_in_this_process_is_never_stale() {
    let (_serial, temp) = temp();
    let path = temp.path().join("d.yaml");
    write_doc(&path, "a", 1);
    assert_eq!(
        read_yaml_atomic_cached::<Doc>(&path).expect("first").title,
        "a"
    );

    // store_serialized is what a writer calls after committing; it models the
    // exact sequence GroupStore::save -> GroupStore::load.
    write_doc(&path, "written", 7);
    store_serialized(
        &path,
        &Doc {
            title: "written".into(),
            count: 7,
        },
    );
    let after = read_yaml_atomic_cached::<Doc>(&path).expect("after write");
    assert_eq!(after.title, "written");
    assert_eq!(after.count, 7);
}

#[test]
fn atomic_rename_by_an_external_writer_is_observed() {
    let (_serial, temp) = temp();
    let path = temp.path().join("d.yaml");
    write_doc(&path, "a", 1);
    assert_eq!(
        read_yaml_atomic_cached::<Doc>(&path).expect("first").count,
        1
    );

    // Another process (or another GroupStore) replaces the file through the
    // normal atomic writer: new inode, so the gate must reopen.
    write_doc(&path, "b", 2);
    let after = read_yaml_atomic_cached::<Doc>(&path).expect("after rename");
    assert_eq!(after.title, "b");
    assert_eq!(after.count, 2);
}

#[test]
fn a_missing_file_is_not_cached() {
    let (_serial, temp) = temp();
    let path = temp.path().join("absent.yaml");
    assert!(read_yaml_atomic_cached::<Doc>(&path).is_err());
    write_doc(&path, "appeared", 3);
    let now = read_yaml_atomic_cached::<Doc>(&path).expect("appeared later");
    assert_eq!(now.title, "appeared");
}

#[test]
fn a_corrupt_file_reports_its_error_and_does_not_poison_the_cache() {
    let (_serial, temp) = temp();
    let path = temp.path().join("d.yaml");
    write_doc(&path, "a", 1);
    assert!(read_yaml_atomic_cached::<Doc>(&path).is_ok());

    std::fs::write(&path, "title: [unclosed").expect("corrupt");
    assert!(
        read_yaml_atomic_cached::<Doc>(&path).is_err(),
        "a parse error must surface"
    );

    // The bad parse must not be cached: once the file is repaired the reader
    // has to see the repair, not the cached failure.
    write_doc(&path, "repaired", 4);
    assert_eq!(
        read_yaml_atomic_cached::<Doc>(&path)
            .expect("repaired")
            .title,
        "repaired"
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_is_not_cached() {
    let (_serial, temp) = temp();
    let real = temp.path().join("real.yaml");
    let link = temp.path().join("link.yaml");
    write_doc(&real, "a", 1);
    std::os::unix::fs::symlink(&real, &link).expect("symlink");

    assert!(read_yaml_atomic_cached::<Doc>(&link).is_ok());
    // Replacing the target behind the symlink must be visible, because a
    // symlinked file is deliberately never cached.
    write_doc(&real, "b", 2);
    assert_eq!(
        read_yaml_atomic_cached::<Doc>(&link).expect("after").title,
        "b"
    );
}

#[test]
fn a_delayed_writer_hook_does_not_seed_another_writers_file() {
    let (_serial, temp) = temp();
    let path = temp.path().join("d.yaml");
    write_doc(&path, "old", 1);
    assert_eq!(
        read_yaml_atomic_cached::<Doc>(&path).expect("warm").title,
        "old"
    );
    write_doc(&path, "new", 2);
    store_written(
        &path,
        &Doc {
            title: "old".into(),
            count: 1,
        },
    );
    assert_eq!(
        read_yaml_atomic_cached::<Doc>(&path)
            .expect("new writer")
            .title,
        "new"
    );
}

#[cfg(unix)]
#[test]
fn the_cache_releases_excess_paths_instead_of_growing_forever() {
    let (_serial, temp) = temp();
    for index in 0..=MAX_ENTRIES {
        let path = temp.path().join(format!("{index}.yaml"));
        write_doc(&path, "bounded", index as u32);
        read_yaml_atomic_cached::<Doc>(&path).expect("read");
    }
    let guard = cache().lock().expect("cache");
    assert!(
        guard
            .keys()
            .filter(|path| path.starts_with(temp.path()))
            .count()
            <= MAX_ENTRIES
    );
    assert!(
        guard
            .values()
            .map(|entry| entry.estimated_bytes)
            .sum::<usize>()
            <= MAX_ESTIMATED_BYTES
    );
}

#[cfg(unix)]
#[test]
fn the_cache_evicts_by_document_weight_before_hitting_the_path_limit() {
    let (_serial, temp) = temp();
    let title = "x".repeat(32 * 1024);
    let count = 18;
    for index in 0..count {
        let path = temp.path().join(format!("{index}.yaml"));
        write_doc(&path, &title, index as u32);
        assert_eq!(
            read_yaml_atomic_cached::<Doc>(&path).expect("read").count,
            index as u32
        );
    }
    let guard = cache().lock().expect("cache");
    assert!(
        guard
            .keys()
            .filter(|path| path.starts_with(temp.path()))
            .count()
            < count
    );
    assert!(
        guard
            .values()
            .map(|entry| entry.estimated_bytes)
            .sum::<usize>()
            <= MAX_ESTIMATED_BYTES
    );
}

#[cfg(unix)]
#[test]
fn an_oversized_document_is_read_without_being_retained() {
    let (_serial, temp) = temp();
    let path = temp.path().join("large.yaml");
    write_doc(&path, &"x".repeat(MAX_ESTIMATED_BYTES / 8), 1);
    let doc = read_yaml_atomic_cached::<Doc>(&path).expect("read oversized");
    assert_eq!(doc.count, 1);
    assert!(!cache().lock().expect("cache").contains_key(&path));
}

#[test]
fn invalidate_drops_one_path_and_clear_drops_all() {
    let (_serial, temp) = temp();
    let a = temp.path().join("a.yaml");
    let b = temp.path().join("b.yaml");
    write_doc(&a, "a", 1);
    write_doc(&b, "b", 1);
    assert!(read_yaml_atomic_cached::<Doc>(&a).is_ok());
    assert!(read_yaml_atomic_cached::<Doc>(&b).is_ok());

    invalidate(&a);
    std::fs::write(&a, "title: a2\ncount: 2\n").expect("edit a");
    assert_eq!(read_yaml_atomic_cached::<Doc>(&a).expect("a").title, "a2");

    clear();
    std::fs::write(&b, "title: b2\ncount: 2\n").expect("edit b");
    assert_eq!(read_yaml_atomic_cached::<Doc>(&b).expect("b").title, "b2");
}
