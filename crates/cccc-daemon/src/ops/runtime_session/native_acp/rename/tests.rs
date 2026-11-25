use super::*;
use cccc_contracts::DaemonRequest;

const OLD: &str = "original-actor";
const NEW: &str = "renamed-actor";
const ORIGINAL: &str = "original-session";
const LEGACY: &str = "legacy-session";
const RUNTIME: ActorRuntime = ActorRuntime::Devin;

struct Fixture {
    temp: tempfile::TempDir,
    home: HomeLayout,
    group: String,
    command: Vec<String>,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let group = GroupStore::new(home.clone())
            .expect("store")
            .create("ACP rename fixture", "")
            .expect("create group")
            .group_id;
        let fixture = Self {
            temp,
            home,
            group,
            command: vec!["fixture".into(), "acp".into()],
        };
        fixture.call("actor_add", json!({"actor_id":OLD,"runtime":"custom"}));
        let (identity, parts) = fixture.identity(OLD);
        record(&fixture.acp(OLD), &identity, &parts, ORIGINAL, true).expect("seed ACP");
        fs::write_json(&fixture.primary(OLD), &primary(OLD, ORIGINAL)).expect("seed primary");
        fixture
    }

    fn call(&self, op: &str, mut args: Value) -> Value {
        args["group_id"] = json!(self.group);
        let response = crate::handle_request(
            &self.home,
            &DaemonRequest {
                v: 1,
                op: op.into(),
                args: args.as_object().expect("args").clone(),
            },
        );
        assert!(response.ok, "{:?}", response.error);
        Value::Object(response.result)
    }

    fn rename(&self) -> Event {
        let response = self.call("actor_rename", json!({"actor_id":OLD,"new_actor_id":NEW}));
        serde_json::from_value(response["event"].clone()).expect("rename event")
    }

    fn environment(actor: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("CCCC_ACTOR_ID".into(), actor.into()),
            ("FIXTURE_SETTING".into(), "original".into()),
        ])
    }

    fn identity(&self, actor: &str) -> (Value, LaunchParts) {
        launch(self.temp.path(), &self.command, &Self::environment(actor))
    }

    fn acp(&self, actor: &str) -> PathBuf {
        receipt_path(&self.home, &self.group, actor, RUNTIME).expect("ACP path")
    }

    fn primary(&self, actor: &str) -> PathBuf {
        path(&self.home, &self.group, actor).expect("primary path")
    }

    fn checkpoint(&self) -> PathBuf {
        checkpoint_path(&self.home, &self.group, NEW).expect("checkpoint path")
    }

    fn group(&self) -> GroupDoc {
        GroupStore::new(self.home.clone())
            .expect("store")
            .load(&self.group)
            .expect("group")
    }

    fn legacy_record(&self, note_replacement: bool) -> Value {
        let (identity, parts) = self.identity(NEW);
        let mut legacy = receipt(&identity, &parts, LEGACY, true);
        legacy
            .as_object_mut()
            .expect("receipt")
            .remove(RENAME_AWARE);
        if note_replacement {
            replaced::write_recorded(&self.acp(NEW), legacy).expect("legacy replacement");
        } else {
            fs::write_json(&self.acp(NEW), &legacy).expect("legacy new session");
        }
        fs::read_json(&self.acp(NEW)).expect("read legacy receipt")
    }

    fn recover(&self, environment: &BTreeMap<String, String>) -> Option<LegacyRecovery> {
        recover_legacy(
            &self.home,
            &self.group,
            NEW,
            RUNTIME,
            self.temp.path(),
            &self.command,
            environment,
        )
        .expect("check recovery")
    }
}

fn primary(actor: &str, session: &str) -> Value {
    json!({"actor_id":actor,"provider_session_id":session,"runtime":"devin","transport":"acp"})
}

#[test]
fn actor_rename_checkpoints_native_acp_before_moving_and_keeps_it_on_retry() {
    let fixture = Fixture::new();
    let original = std_fs::read(fixture.acp(OLD)).expect("read original receipt");
    let event = fixture.rename();
    let checkpoint_file = fixture.checkpoint();
    let saved_bytes = std_fs::read(&checkpoint_file).expect("read checkpoint");
    let saved: Checkpoint = serde_json::from_slice(&saved_bytes).expect("checkpoint JSON");
    assert_eq!(saved.rename, event);
    assert_eq!(
        saved.receipts["devin"],
        serde_json::from_slice::<Value>(&original).expect("original receipt JSON")
    );
    assert_eq!(saved.primary, Some(primary(OLD, ORIGINAL)));
    assert!(!fixture.acp(OLD).exists());
    assert_eq!(std_fs::read(fixture.acp(NEW)).expect("moved ACP"), original);
    checkpoint(&fixture.home, &fixture.group(), OLD, NEW, &event).expect("retry checkpoint");
    assert_eq!(
        std_fs::read(&checkpoint_file).expect("checkpoint after retry"),
        saved_bytes
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std_fs::metadata(checkpoint_file)
                .expect("checkpoint mode")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[cfg(unix)]
fn owned_binding() -> owned_mcp::Binding {
    owned_mcp::Binding {
        owner_id: uuid::Uuid::new_v4().simple().to_string(),
        instance: uuid::Uuid::new_v4().simple().to_string(),
        launch_id: uuid::Uuid::new_v4().simple().to_string(),
    }
}

#[cfg(unix)]
fn legacy_locator_pair(fixture: &Fixture) -> (PathBuf, PathBuf, owned_mcp::Binding) {
    GroupStore::new(fixture.home.clone())
        .expect("store")
        .mutate(&fixture.group, |group| {
            group.actors[0].runtime = RUNTIME;
            group.actors[0].runtime_mode = cccc_contracts::RuntimeMode::Acp;
            Ok(())
        })
        .expect("use native ACP");
    let before = fixture.group();
    let created = &before.actors[0].created_at;
    let old = owned_mcp::binding_file(&fixture.home, &before, OLD, created).expect("old locator");
    let new = owned_mcp::binding_file(&fixture.home, &before, NEW, created).expect("new locator");
    fs::write_secret_json(&old, &owned_binding()).expect("original binding");
    fixture.rename();
    let legacy = owned_binding();
    fs::write_secret_json(&new, &legacy).expect("legacy duplicate");
    (old, new, legacy)
}

#[cfg(unix)]
#[test]
fn actor_rename_reconciles_verified_native_acp_locator_pair() {
    let fixture = Fixture::new();
    let (old, new, legacy) = legacy_locator_pair(&fixture);
    fixture.legacy_record(true);
    let group = fixture.group();
    let actor = &group.actors[0];
    let resolve = || owned_mcp::binding_file(&fixture.home, &group, NEW, &actor.created_at);
    assert!(
        resolve()
            .expect_err("ambiguous pair")
            .to_string()
            .contains("ambiguous")
    );
    let original = owned_mcp::Binding::read_file(&old).expect("original binding");
    let saved: Checkpoint = fs::read_json(&fixture.checkpoint()).expect("checkpoint");
    assert_eq!(
        saved.mcp_locator.as_deref(),
        old.file_name().and_then(std::ffi::OsStr::to_str)
    );

    reconcile_mcp_binding(&fixture.home, &group, actor).expect("verified switchback");
    reconcile_mcp_binding(&fixture.home, &group, actor).expect("idempotent retry");
    assert_eq!(resolve().expect("original locator"), old);
    assert_eq!(
        owned_mcp::Binding::read_file(&old).expect("preserved binding"),
        original
    );
    assert!(!new.exists());
    let archived: Vec<owned_mcp::Binding> =
        std_fs::read_dir(fixture.checkpoint().parent().expect("checkpoint directory"))
            .expect("checkpoint entries")
            .map(|entry| entry.expect("checkpoint entry"))
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".mcp-legacy-")
            })
            .map(|entry| fs::read_json(&entry.path()).expect("archived duplicate"))
            .collect();
    assert_eq!(archived, [legacy]);
    assert_eq!(
        fixture
            .recover(&Fixture::environment(NEW))
            .expect("ACP restore stays eligible")
            .session_id,
        ORIGINAL
    );
}

#[cfg(unix)]
#[test]
fn actor_rename_refuses_unverified_or_multiple_native_acp_locators() {
    let fixture = Fixture::new();
    let (old, new, legacy) = legacy_locator_pair(&fixture);
    let mut group = fixture.group();
    let actor = group.actors[0].clone();
    let original_receipt: Value = fs::read_json(&fixture.acp(NEW)).expect("original ACP receipt");
    fixture.legacy_record(false);
    reconcile_mcp_binding(&fixture.home, &group, &actor).expect("no verified replacement");
    assert!(owned_mcp::binding_file(&fixture.home, &group, NEW, &actor.created_at).is_err());
    assert_eq!(
        owned_mcp::Binding::read_file(&new).expect("duplicate untouched"),
        legacy
    );

    // An unrelated generation still cannot authorize a legacy replacement.
    fs::write_json(&fixture.acp(NEW), &original_receipt).expect("restore original fixture");
    fixture.legacy_record(true);
    let mut unrelated = actor.clone();
    unrelated.generation = "unrelated-generation".into();
    reconcile_mcp_binding(&fixture.home, &group, &unrelated).expect("no verified lineage");
    assert!(new.exists());

    let extra = "extra-alias";
    group.actor_aliases.insert(extra.into(), NEW.into());
    let extra_file = owned_mcp::binding_file(&fixture.home, &group, extra, &actor.created_at)
        .expect("second former locator");
    fs::write_secret_json(&extra_file, &owned_binding()).expect("second former binding");
    assert!(reconcile_mcp_binding(&fixture.home, &group, &actor).is_err());
    assert!(old.exists() && new.exists() && extra_file.exists());
}

#[test]
fn actor_rename_restores_native_acp_pin_after_legacy_overwrite() {
    let fixture = Fixture::new();
    fixture.rename();
    let legacy = fixture.legacy_record(true);
    let legacy_primary = primary(NEW, LEGACY);
    fs::write_json(&fixture.primary(NEW), &legacy_primary).expect("legacy primary overwrite");
    let (identity, parts) = fixture.identity(NEW);
    assert_eq!(
        prepare(&fixture.acp(NEW), &identity, &parts)
            .expect("legacy pin")
            .as_deref(),
        Some(LEGACY)
    );
    let recovery = fixture
        .recover(&Fixture::environment(NEW))
        .expect("eligible recovery");
    assert_eq!(recovery.session_id, ORIGINAL);
    let archive = recovery.archive.clone();
    recovery
        .record(&fixture.acp(NEW), &identity, &parts, ORIGINAL, true)
        .expect("provider confirmed resume");
    assert_eq!(
        prepare(&fixture.acp(NEW), &identity, &parts)
            .expect("restored pin")
            .as_deref(),
        Some(ORIGINAL)
    );
    assert_eq!(
        fs::read_json::<Value>(&fixture.primary(NEW)).expect("restored primary"),
        primary(NEW, ORIGINAL)
    );
    assert_eq!(
        fs::read_json::<Value>(&archive).expect("legacy archive"),
        json!({"acp":legacy,"primary":legacy_primary})
    );
    assert!(fixture.recover(&Fixture::environment(NEW)).is_none());
}

#[test]
fn actor_rename_checkpoint_respects_new_session_and_later_forward_receipts() {
    let fixture = Fixture::new();
    fixture.rename();
    let (identity, parts) = fixture.identity(NEW);
    record(&fixture.acp(NEW), &identity, &parts, LEGACY, true).expect("later forward receipt");
    assert!(fixture.recover(&Fixture::environment(NEW)).is_none());
    std_fs::remove_file(fixture.acp(NEW)).expect("type New Session");
    assert!(fixture.recover(&Fixture::environment(NEW)).is_none());
    fixture.legacy_record(false);
    assert!(fixture.recover(&Fixture::environment(NEW)).is_none());
}

#[test]
fn actor_rename_checkpoint_leaves_unrelated_primary_receipts_out_of_acp_recovery() {
    let fixture = Fixture::new();
    let other_transport = json!({"actor_id":OLD,"provider_session_id":ORIGINAL,"runtime":"pi"});
    fs::write_json(&fixture.primary(OLD), &other_transport).expect("non-ACP primary");
    fixture.rename();
    let saved: Checkpoint = fs::read_json(&fixture.checkpoint()).expect("checkpoint");
    assert!(saved.primary.is_none());
    fixture.legacy_record(true);
    let (identity, parts) = fixture.identity(NEW);
    fixture
        .recover(&Fixture::environment(NEW))
        .expect("eligible recovery")
        .record(&fixture.acp(NEW), &identity, &parts, ORIGINAL, true)
        .expect("recover ACP");
    let mut expected = other_transport;
    expected["actor_id"] = json!(NEW);
    assert_eq!(
        fs::read_json::<Value>(&fixture.primary(NEW)).expect("non-ACP primary"),
        expected
    );
}

#[test]
fn actor_rename_checkpoint_requires_matching_launch_and_actor_generation() {
    let fixture = Fixture::new();
    fixture.rename();
    fixture.legacy_record(true);
    let mut environment = Fixture::environment(NEW);
    environment.insert("FIXTURE_SETTING".into(), "changed".into());
    assert!(fixture.recover(&environment).is_none());
    GroupStore::new(fixture.home.clone())
        .expect("store")
        .mutate(&fixture.group, |group| {
            group.actors[0].generation = "different-generation".into();
            Ok(())
        })
        .expect("change actor generation");
    assert!(fixture.recover(&Fixture::environment(NEW)).is_none());
}

#[test]
fn actor_rename_recovery_waits_for_provider_resume_before_replacing_receipts() {
    let fixture = Fixture::new();
    fixture.rename();
    let legacy = fixture.legacy_record(true);
    let (identity, parts) = fixture.identity(NEW);
    let recovery = fixture
        .recover(&Fixture::environment(NEW))
        .expect("eligible recovery");
    let archive = recovery.archive.clone();
    assert!(
        recovery
            .record(&fixture.acp(NEW), &identity, &parts, LEGACY, false)
            .is_err()
    );
    assert!(!archive.exists());
    assert_eq!(
        fs::read_json::<Value>(&fixture.acp(NEW)).expect("active ACP"),
        legacy
    );
    assert!(fixture.recover(&Fixture::environment(NEW)).is_some());
}
