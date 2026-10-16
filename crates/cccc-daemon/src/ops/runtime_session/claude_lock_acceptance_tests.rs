use super::*;
const SAVED: &str = "52b41c61-e23c-4b7c-8b60-809c347451b5";
struct Fixture {
    _temp: tempfile::TempDir,
    home: HomeLayout,
    group: String,
    cwd: std::path::PathBuf,
    command: Vec<String>,
    env: BTreeMap<String, String>,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("lock error fixture");
        let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
        let group = cccc_core::GroupStore::new(home.clone())
            .expect("store")
            .create("lock admission", "")
            .expect("group")
            .group_id;
        let cwd = temp.path().join("workspace");
        std::fs::create_dir(&cwd).expect("workspace");
        Self {
            _temp: temp,
            home,
            group,
            cwd,
            command: vec!["claude".into(), "--model".into(), "sonnet".into()],
            env: BTreeMap::new(),
        }
    }
    fn path(&self) -> std::path::PathBuf {
        super::super::path(&self.home, &self.group, "actor").expect("receipt path")
    }
    fn record(&self) {
        record_managed(
            &self.home,
            &self.group,
            "actor",
            &self.cwd,
            &self.command,
            &self.env,
            SAVED,
            false,
        )
        .expect("usable receipt");
    }
    fn fail_lock(&self) {
        let lock = self.path().with_extension("lock");
        if lock.is_file() {
            std::fs::remove_file(&lock).expect("remove fixture lock");
        }
        std::fs::create_dir_all(lock).expect("directory at lock path");
    }
    fn prepare(&self) -> io::Result<Option<ResumeAttempt>> {
        prepare_managed(
            &self.home,
            &self.group,
            "actor",
            &self.cwd,
            &self.command,
            &self.env,
        )
    }
    fn retry(&self) -> io::Result<bool> {
        retry_failed_resume_for_command(&self.home, &self.group, "actor", &self.command)
    }
    fn document(&self) -> Map<String, Value> {
        read(&self.home, &self.group, "actor").expect("receipt")
    }
}
#[test]
fn lock_admission_io_error_skips_only_optional_absent_corrupt_or_usable_resume() {
    for state in ["absent", "corrupt", "usable"] {
        let f = Fixture::new();
        if state == "usable" {
            f.record();
        }
        if state == "corrupt" {
            std::fs::create_dir_all(f.path().parent().expect("parent")).expect("receipt dir");
            std::fs::write(f.path(), b"{bad-json").expect("corrupt receipt");
        }
        let before = std::fs::read(f.path()).ok();
        f.fail_lock();
        assert!(
            f.prepare()
                .expect("optional resume lock error must choose ordinary fresh policy")
                .is_none(),
            "{state}"
        );
        assert!(
            !f.retry()
                .expect("manual retry lock error must not fail ordinary Start"),
            "{state}"
        );
        assert_eq!(
            std::fs::read(f.path()).ok(),
            before,
            "do not rewrite evidence on lock error"
        );
    }
}
#[test]
fn lock_admission_error_never_bypasses_selected_pending_failed_or_inflight_evidence() {
    for evidence in [
        "selected",
        "pending",
        "failed",
        "inflight",
        "failure_diagnostic",
    ] {
        let f = Fixture::new();
        f.record();
        let mut document = f.document();
        match evidence {
            "selected" => {
                document.insert("explicit_resume_session_id".into(), json!(SAVED));
            }
            "pending" => {
                document.insert("explicit_resume_pending".into(), json!(true));
            }
            "failed" => {
                document.insert("status".into(), json!("resume_failed"));
                document.insert("last_failed_resume_model".into(), json!("sonnet"));
                document.insert("last_resume_error".into(), json!("actual failed resume"));
            }
            "inflight" => {
                document.insert("last_resume_attempt_id".into(), json!("owned-attempt"));
            }
            _ => {
                document.insert("last_resume_error".into(), json!("actual prior failure"));
            }
        }
        write(&f.home, &f.group, "actor", &document).expect("strict evidence");
        let before = std::fs::read(f.path()).expect("evidence bytes");
        f.fail_lock();
        assert!(
            f.prepare().is_err(),
            "prepare {evidence} must remain strict"
        );
        assert!(
            f.retry().is_err(),
            "manual retry {evidence} must remain strict"
        );
        assert_eq!(std::fs::read(f.path()).expect("unchanged evidence"), before);
    }
}
#[test]
fn mandatory_attempt_and_retry_write_errors_remain_errors_after_lock_admission() {
    let f = Fixture::new();
    f.record();
    let before = std::fs::read(f.path()).expect("saved bytes");
    super::super::receipt_lock_tests::fail_next_write(&f.path());
    let error = f
        .prepare()
        .err()
        .expect("mandatory attempt write cannot become fresh");
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(error.to_string(), "fixture mandatory receipt write denied");
    assert_eq!(std::fs::read(f.path()).expect("same bytes"), before);
    let attempt = f
        .prepare()
        .expect("real admitted prepare")
        .expect("attempt");
    record_resume_failure(
        &f.home,
        &f.group,
        "actor",
        &attempt,
        "actual failed Sonnet resume",
        true,
    )
    .expect("failure evidence");
    let before = std::fs::read(f.path()).expect("failed bytes");
    super::super::receipt_lock_tests::fail_next_write(&f.path());
    let error = f
        .retry()
        .expect_err("mandatory explicit retry fence cannot be skipped");
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(error.to_string(), "fixture mandatory receipt write denied");
    assert_eq!(
        std::fs::read(f.path()).expect("retained failure bytes"),
        before
    );
}
