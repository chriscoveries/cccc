use super::*;
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::cell::Cell;
use std::os::unix::fs::symlink;

const GROUP: &[u8] = include_bytes!("../tests/fixtures/captured_group.yaml");
const SETTINGS: &[u8] = include_bytes!("../tests/fixtures/captured_settings.yaml");
thread_local! { static DECODES: Cell<usize> = const { Cell::new(0) }; }

#[derive(Clone, Debug, PartialEq)]
struct Counted(Value);

impl<'de> Deserialize<'de> for Counted {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        DECODES.with(|count| count.set(count.get() + 1));
        Value::deserialize(deserializer).map(Self)
    }
}

fn count() -> usize {
    DECODES.with(Cell::get)
}

fn fixture(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, bytes).expect("captured configuration");
    path
}

#[test]
fn unchanged_capture_decodes_once_and_returns_independent_values() {
    let dir = tempfile::tempdir().expect("directory");
    let path = fixture(dir.path(), "group.yaml", GROUP);
    let start = count();
    let mut first = read_yaml::<Counted>(&path).expect("group");
    first.0["title"] = Value::Null;
    let second = read_yaml::<Counted>(&path).expect("group again");
    assert_eq!(
        count() - start,
        1,
        "unchanged bytes must reuse their decode"
    );
    assert_ne!(first, second, "callers must own independent values");
    assert_eq!(
        read_yaml::<crate::GroupDoc>(&path)
            .expect("typed group")
            .title,
        "[redacted]"
    );
    let other = fixture(dir.path(), "settings.yaml", SETTINGS);
    let settings = read_yaml::<crate::settings::GlobalSettings>(&other).expect("typed settings");
    assert_eq!(
        settings.observability["terminal_transcript"]["per_actor_bytes"],
        2000000
    );
}

#[test]
fn equal_length_preserved_mtime_edit_and_torn_content_are_observed() {
    let dir = tempfile::tempdir().expect("directory");
    let path = fixture(dir.path(), "group.yaml", GROUP);
    let original = read_yaml::<Counted>(&path).expect("original");
    let mtime = fs::metadata(&path)
        .expect("metadata")
        .modified()
        .expect("mtime");
    let edited =
        String::from_utf8(GROUP.to_vec())
            .expect("UTF-8")
            .replacen("[redacted]", "[REDACTED]", 1);
    assert_eq!(edited.len(), GROUP.len());
    fs::write(&path, edited.as_bytes()).expect("edit");
    File::options()
        .write(true)
        .open(&path)
        .expect("file")
        .set_modified(mtime)
        .expect("restore mtime");
    assert_ne!(read_yaml::<Counted>(&path).expect("edited"), original);
    fs::write(&path, &GROUP[..GROUP.len() / 2]).expect("torn write control");
    assert!(
        read_yaml::<Counted>(&path).is_err(),
        "malformed bytes must not use an old value"
    );
    fs::write(&path, GROUP).expect("restore capture");
    assert_eq!(read_yaml::<Counted>(&path).expect("restored"), original);
    fs::remove_file(&path).expect("remove");
    assert!(
        read_yaml::<Counted>(&path).is_err(),
        "missing files must remain visible"
    );
}

#[test]
fn replacement_symlink_and_oversized_files_do_not_reuse_stale_decodes() {
    let dir = tempfile::tempdir().expect("directory");
    let path = fixture(dir.path(), "group.yaml", GROUP);
    read_yaml::<Counted>(&path).expect("first");
    let start = count();
    crate::fs::atomic_write(&path, GROUP).expect("atomic replacement");
    read_yaml::<Counted>(&path).expect("new identity");
    assert_eq!(
        count() - start,
        1,
        "replacement must not reuse the previous inode's decode"
    );
    let link = dir.path().join("link.yaml");
    symlink(&path, &link).expect("link");
    let start = count();
    read_yaml::<Counted>(&link).expect("symlink fallback");
    read_yaml::<Counted>(&link).expect("symlink fallback again");
    assert_eq!(count() - start, 2);
    let mut padded = SETTINGS.to_vec();
    padded.resize(MAX_FILE_BYTES + 1, b' ');
    let large = fixture(dir.path(), "large.yaml", &padded);
    let start = count();
    read_yaml::<Counted>(&large).expect("large fallback");
    read_yaml::<Counted>(&large).expect("large fallback again");
    assert_eq!(count() - start, 2);
    assert!(read_yaml::<Counted>(dir.path()).is_err());
}

#[test]
fn committed_writes_invalidate_without_seeding_a_later_read() {
    let dir = tempfile::tempdir().expect("directory");
    let path = fixture(dir.path(), "settings.yaml", SETTINGS);
    let settings = read_yaml::<Counted>(&path).expect("settings");
    let original_file = File::open(&path).expect("old file");
    let old_identity = Handle::from_file(original_file).expect("identity");
    let value: Value = serde_yaml::from_slice(SETTINGS).expect("capture");
    crate::fs::write_yaml(&path, &value).expect("write");
    let start = count();
    read_yaml::<Counted>(&path).expect("committed read");
    assert_eq!(count() - start, 1);
    let stale = Entry {
        identity: old_identity,
        bytes: SETTINGS.to_vec(),
        value: Arc::new(settings),
        weight: SETTINGS.len() * 16,
        used: 0,
    };
    let retired = cache()
        .lock()
        .expect("cache")
        .insert((path.clone(), TypeId::of::<Counted>()), stale);
    drop(retired);
    let start = count();
    assert_eq!(
        read_yaml::<Counted>(&path)
            .expect("after delayed insertion")
            .0,
        value
    );
    assert_eq!(
        count() - start,
        1,
        "an old reader must not seed a hit for a new file"
    );
    crate::fs::write_yaml_committed(&path, &value).expect("committed writer");
    let start = count();
    read_yaml::<Counted>(&path).expect("after committed writer");
    assert_eq!(count() - start, 1);
    // A failed writer must not hide its error or seed the proposed value.
    assert!(crate::fs::write_yaml(dir.path(), &value).is_err());
}

#[test]
fn captured_documents_survive_canonical_writers_and_settings_migration() {
    let dir = tempfile::tempdir().expect("directory");
    let home = crate::HomeLayout::from_path(dir.path()).expect("home");
    let store = crate::GroupStore::new(home.clone()).expect("store");
    let group: crate::GroupDoc = serde_yaml::from_slice(GROUP).expect("captured group");
    store.save(&group).expect("canonical group writer");
    let loaded = store.load(&group.group_id).expect("group load");
    assert_eq!(loaded.title, group.title);
    assert_eq!(loaded.created_at, group.created_at);
    fixture(dir.path(), "settings.json", SETTINGS);
    let migrated = crate::settings::load(&home).expect("legacy settings migration");
    assert!(dir.path().join(".rust-settings-migrated-v2").exists());
    let expected: crate::settings::GlobalSettings =
        serde_yaml::from_slice(SETTINGS).expect("capture");
    assert_eq!(migrated.observability, expected.observability);
    crate::settings::save(&home, &migrated).expect("canonical settings writer");
    crate::settings::update(&home, |settings| {
        assert_eq!(settings.observability, expected.observability);
        Ok(())
    })
    .expect("canonical update");
    assert_eq!(
        crate::settings::load(&home)
            .expect("read after update")
            .observability,
        expected.observability
    );
}

#[test]
fn slot_and_estimated_weight_limits_evict_oldest_entries() {
    let dir = tempfile::tempdir().expect("directory");
    let path = fixture(dir.path(), "group.yaml", GROUP);
    let mut slots = Cache::default();
    for i in 0..=MAX_ENTRIES {
        let entry = Entry {
            identity: Handle::from_file(File::open(&path).expect("file")).expect("identity"),
            bytes: GROUP.to_vec(),
            value: Arc::new(serde_json::from_slice::<Value>(GROUP).expect("capture")),
            weight: GROUP.len() * 16,
            used: 0,
        };
        drop(slots.insert(
            (dir.path().join(i.to_string()), TypeId::of::<Value>()),
            entry,
        ));
    }
    assert_eq!(slots.entries.len(), MAX_ENTRIES);
    assert!(
        !slots
            .entries
            .contains_key(&(dir.path().join("0"), TypeId::of::<Value>()))
    );
    let huge = Entry {
        identity: Handle::from_file(File::open(&path).expect("file")).expect("identity"),
        bytes: GROUP.to_vec(),
        value: Arc::new(Value::Null),
        weight: MAX_WEIGHT + 1,
        used: 0,
    };
    drop(slots.insert((path, TypeId::of::<Value>()), huge));
    assert!(slots.weight <= MAX_WEIGHT);
    assert!(slots.entries.len() <= MAX_ENTRIES);
}
