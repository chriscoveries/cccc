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

fn temp() -> tempfile::TempDir {
    clear();
    tempfile::tempdir().expect("tempdir")
}

fn write_doc(path: &Path, title: &str, count: u32) {
    crate::fs::write_yaml(path, &Doc { title: title.into(), count }).expect("write");
}

/// Documents the one case this cache deliberately does not cover, so the
/// limitation is a pinned decision rather than a latent surprise. See the
/// module's `Contract` section.
///
/// A same-length in-place rewrite inside a single filesystem mtime tick keeps
/// all four identity fields identical and is therefore invisible. That is the
/// price of not reading the file, and it is why the entry point is named for
/// atomic writers only.
#[test]
fn same_length_in_place_edit_within_one_mtime_tick_is_the_documented_gap() {
    let temp = temp();
    let path = temp.path().join("d.yaml");
    write_doc(&path, "a", 1);
    let first = read_yaml_atomic_cached::<Doc>(&path).expect("first");

    // An external tool rewrites in place, same length, immediately.
    let before = FileIdentity::of(&std::fs::metadata(&path).expect("meta"));
    std::fs::write(&path, "title: b\ncount: 2\n").expect("in-place edit");
    let after = FileIdentity::of(&std::fs::metadata(&path).expect("meta"));

    let second = read_yaml_atomic_cached::<Doc>(&path).expect("second");
    if before == after {
        // Identity did not change at all, so the cached parse is reused. This
        // is the documented gap; observing it here is the point.
        assert_eq!(second.title, first.title);
        assert_eq!(second.count, first.count);
    } else {
        // Identity changed, so the edit is necessarily observed.
        assert_eq!(second.title, "b");
        assert_eq!(second.count, 2);
    }
}

/// The guarantee that does hold: any rename-based write is always seen, even
/// when the new content happens to be the same length.
#[test]
fn an_atomic_write_is_always_observed_even_at_equal_length() {
    let temp = temp();
    let path = temp.path().join("d.yaml");
    write_doc(&path, "a", 1);
    assert_eq!(read_yaml_atomic_cached::<Doc>(&path).expect("first").title, "a");

    write_doc(&path, "b", 2); // "title: b\ncount: 2" — same byte length as "a"/1
    assert_eq!(read_yaml_atomic_cached::<Doc>(&path).expect("after").title, "b");
}

#[test]
fn caller_receives_an_independent_copy() {
    let temp = temp();
    let path = temp.path().join("d.yaml");
    write_doc(&path, "a", 1);

    let mut first = read_yaml_atomic_cached::<Doc>(&path).expect("first");
    first.count = 99;
    let second = read_yaml_atomic_cached::<Doc>(&path).expect("second");
    assert_eq!(second.count, 1, "mutating a caller's copy must not affect the cache");
}

#[test]
fn read_after_write_in_this_process_is_never_stale() {
    let temp = temp();
    let path = temp.path().join("d.yaml");
    write_doc(&path, "a", 1);
    assert_eq!(read_yaml_atomic_cached::<Doc>(&path).expect("first").title, "a");

    // store_serialized is what a writer calls after committing; it models the
    // exact sequence GroupStore::save -> GroupStore::load.
    write_doc(&path, "written", 7);
    store_serialized(
        &path,
        &Doc { title: "written".into(), count: 7 },
    );
    let after = read_yaml_atomic_cached::<Doc>(&path).expect("after write");
    assert_eq!(after.title, "written");
    assert_eq!(after.count, 7);
}

#[test]
fn atomic_rename_by_an_external_writer_is_observed() {
    let temp = temp();
    let path = temp.path().join("d.yaml");
    write_doc(&path, "a", 1);
    assert_eq!(read_yaml_atomic_cached::<Doc>(&path).expect("first").count, 1);

    // Another process (or another GroupStore) replaces the file through the
    // normal atomic writer: new inode, so the gate must reopen.
    write_doc(&path, "b", 2);
    let after = read_yaml_atomic_cached::<Doc>(&path).expect("after rename");
    assert_eq!(after.title, "b");
    assert_eq!(after.count, 2);
}

#[test]
fn a_missing_file_is_not_cached() {
    let temp = temp();
    let path = temp.path().join("absent.yaml");
    assert!(read_yaml_atomic_cached::<Doc>(&path).is_err());
    write_doc(&path, "appeared", 3);
    let now = read_yaml_atomic_cached::<Doc>(&path).expect("appeared later");
    assert_eq!(now.title, "appeared");
}

#[test]
fn a_corrupt_file_reports_its_error_and_does_not_poison_the_cache() {
    let temp = temp();
    let path = temp.path().join("d.yaml");
    write_doc(&path, "a", 1);
    assert!(read_yaml_atomic_cached::<Doc>(&path).is_ok());

    std::fs::write(&path, "title: [unclosed").expect("corrupt");
    assert!(read_yaml_atomic_cached::<Doc>(&path).is_err(), "a parse error must surface");

    // The bad parse must not be cached: once the file is repaired the reader
    // has to see the repair, not the cached failure.
    write_doc(&path, "repaired", 4);
    assert_eq!(read_yaml_atomic_cached::<Doc>(&path).expect("repaired").title, "repaired");
}

#[test]
fn a_symlink_is_not_cached() {
    let temp = temp();
    let real = temp.path().join("real.yaml");
    let link = temp.path().join("link.yaml");
    write_doc(&real, "a", 1);
    std::os::unix::fs::symlink(&real, &link).expect("symlink");

    assert!(read_yaml_atomic_cached::<Doc>(&link).is_ok());
    // Replacing the target behind the symlink must be visible, because a
    // symlinked file is deliberately never cached.
    write_doc(&real, "b", 2);
    assert_eq!(read_yaml_atomic_cached::<Doc>(&link).expect("after").title, "b");
}

#[test]
fn invalidate_drops_one_path_and_clear_drops_all() {
    let temp = temp();
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
