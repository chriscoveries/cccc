use super::*;
#[test]
fn retarget_receipt_read_must_acquire_the_receipt_lock_before_native_preflight() {
    let temp = tempfile::tempdir().expect("retarget lock fixture");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("home");
    let group = cccc_core::GroupStore::new(home.clone())
        .expect("store")
        .create("retarget lock", "")
        .expect("group")
        .group_id;
    let cwd = temp.path().join("workspace");
    let config = temp.path().join("claude-config");
    std::fs::create_dir(&cwd).expect("workspace");
    std::fs::create_dir(&config).expect("config without transcripts");
    let command = vec!["claude".into()];
    let env = BTreeMap::from([(
        "CLAUDE_CONFIG_DIR".into(),
        config.to_string_lossy().into_owned(),
    )]);
    const ID: &str = "52b41c61-e23c-4b7c-8b60-809c347451b5";
    super::super::record_managed(&home, &group, "actor", &cwd, &command, &env, ID, false)
        .expect("receipt");
    let path = super::super::super::path(&home, &group, "actor").expect("receipt path");
    let before = std::fs::read(&path).expect("saved bytes");
    let lock = path.with_extension("lock");
    std::fs::remove_file(&lock).expect("remove fixture lock");
    std::fs::create_dir(&lock).expect("inject lock admission IO error");
    let error = validate_retarget(&home, &group, "actor", &cwd, &command, &env, ID)
        .err()
        .expect("read admission must fail");
    assert_eq!(
        error.code, "claude_receipt_missing",
        "fail at the locked receipt read, before native transcript preflight"
    );
    assert_eq!(std::fs::read(path).expect("unchanged receipt"), before);
}
