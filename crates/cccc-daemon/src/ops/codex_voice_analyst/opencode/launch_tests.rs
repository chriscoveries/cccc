use super::*;
use cccc_contracts::ActorRuntime;
use std::fs;

use std::path::PathBuf;

/// Canonical form of a path the launcher derives through `canonicalize`
/// (native cache, tree-sitter resources): on Windows with a `\\?\` verbatim
/// prefix, and anywhere with symlinks resolved (macOS /tmp). Expectations on
/// those derived paths must canonicalize the same way; plain launch argv is
/// never canonicalized and keeps verbatim expectations.
fn canonical(path: &Path) -> String {
    fs::canonicalize(path)
        .map(|resolved| resolved.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}

fn npm_install(root: &Path, local: bool) -> (PathBuf, PathBuf) {
    let modules = root.join("node_modules");
    let bin = if local {
        modules.join(".bin")
    } else {
        root.to_owned()
    };
    let script = modules.join("@kilocode/cli/bin/kilo");
    fs::create_dir_all(script.parent().expect("script directory")).expect("package directory");
    fs::create_dir_all(&bin).expect("shim directory");
    fs::write(&script, "#!/usr/bin/env node\n").expect("npm entrypoint");
    let relative = if local {
        r"..\@kilocode\cli\bin\kilo"
    } else {
        r"node_modules\@kilocode\cli\bin\kilo"
    };
    let shim = bin.join("kilo.cmd");
    fs::write(
        &shim,
        format!("@ECHO off\r\n\"%~dp0\\node.exe\" \"%~dp0\\{relative}\" %*\r\n"),
    )
    .expect("npm shim");
    fs::write(bin.join("node.exe"), "fixture").expect("bundled node fixture");
    (shim, script)
}

#[test]
fn windows_npm_kilo_global_and_local_installations_prepare() {
    for local in [false, true] {
        let temp = tempfile::tempdir().expect("tempdir");
        let (shim, script) = npm_install(&temp.path().join("npm & tools 空间"), local);
        let prepared = prepare(
            &[
                shim.to_string_lossy().into_owned(),
                "--model=provider/model".into(),
                "--pure".into(),
            ],
            &BTreeMap::new(),
            ActorRuntime::Kilo,
        )
        .unwrap_or_else(|error| {
            panic!("official npm install (local={local}) must launch: {error}")
        });
        assert_eq!(prepared.model.as_deref(), Some("provider/model"));
        assert_eq!(prepared.acp_arguments, ["--pure"]);
        assert_eq!(prepared.tui_arguments, ["--pure"]);
        assert_eq!(
            prepared.launch_prefix,
            [
                shim.parent()
                    .expect("shim directory")
                    .join("node.exe")
                    .to_string_lossy()
                    .into_owned(),
                script.to_string_lossy().into_owned(),
            ]
        );
    }
}

#[test]
fn npm_launch_uses_node_from_the_effective_path_when_not_bundled() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (shim, script) = npm_install(temp.path(), false);
    fs::remove_file(temp.path().join("node.exe")).expect("remove bundled node fixture");
    let node_dir = temp.path().join("Node & tools");
    fs::create_dir(&node_dir).expect("PATH node directory");
    let node = node_dir.join("node.exe");
    fs::write(&node, "fixture").expect("PATH node fixture");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&node, fs::Permissions::from_mode(0o700)).expect("executable fixture");
    }
    let env = BTreeMap::from([("PATH".into(), node_dir.to_string_lossy().into_owned())]);
    let prepared = prepare(
        &[shim.to_string_lossy().into_owned()],
        &env,
        ActorRuntime::Kilo,
    )
    .expect("prepare with PATH node");
    assert_eq!(
        prepared.launch_prefix,
        [node.to_string_lossy(), script.to_string_lossy()]
    );
}

#[test]
fn npm_launch_reports_missing_package_or_node_without_executing_the_shim() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (shim, script) = npm_install(temp.path(), false);
    let configured = [shim.to_string_lossy().into_owned()];
    fs::remove_file(temp.path().join("node.exe")).expect("remove bundled node fixture");
    let env = BTreeMap::from([("PATH".into(), temp.path().to_string_lossy().into_owned())]);
    let error = prepare(&configured, &env, ActorRuntime::Kilo)
        .err()
        .expect("missing node must fail");
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert!(error.to_string().contains("node.exe"));
    fs::remove_file(script).expect("remove npm entrypoint fixture");
    let error = prepare(&configured, &env, ActorRuntime::Kilo)
        .err()
        .expect("missing package must fail");
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert!(error.to_string().contains("Kilo npm entrypoint is missing"));
}

#[test]
fn direct_executables_are_unchanged_and_custom_wrappers_stay_unsupported() {
    let temp = tempfile::tempdir().expect("tempdir");
    for (runtime, name) in [
        (ActorRuntime::Kilo, "kilo"),
        (ActorRuntime::Opencode, "opencode"),
    ] {
        for filename in [name.to_owned(), format!("{name}.exe")] {
            let executable = temp.path().join(filename).to_string_lossy().into_owned();
            fs::write(&executable, "fixture").expect("direct executable fixture");
            let prepared = prepare(&[executable.clone()], &BTreeMap::new(), runtime)
                .expect("direct executable");
            assert_eq!(prepared.launch_prefix, [executable]);
        }
        let wrapper = temp
            .path()
            .join("custom.cmd")
            .to_string_lossy()
            .into_owned();
        fs::write(&wrapper, "fixture").expect("custom wrapper fixture");
        let error = prepare(&[wrapper], &BTreeMap::new(), runtime)
            .err()
            .expect("unsupported custom wrapper");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}

#[test]
fn official_kilo_npm_cache_bypasses_the_resident_node_shim() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (shim, script) = npm_install(temp.path(), false);
    let cached = script.parent().expect("bin").join(".kilo");
    fs::write(&cached, "native fixture").expect("native cache");
    for executable in [shim, script] {
        let prepared = prepare(
            &[executable.to_string_lossy().into_owned()],
            &BTreeMap::from([("CCCC_OPENCODE_HEADLESS".into(), "1".into())]),
            ActorRuntime::Kilo,
        )
        .expect("native npm cache");
        assert_eq!(prepared.launch_prefix, [canonical(&cached)]);
    }
}

#[cfg(unix)]
#[test]
fn kilo_npm_symlink_uses_the_package_cache_and_override() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (_, script) = npm_install(temp.path(), false);
    let cached = script.parent().expect("bin").join(".kilo");
    fs::write(&cached, "native fixture").expect("native cache");
    let bin = temp.path().join("bin");
    fs::create_dir(&bin).expect("bin");
    let link = bin.join("kilo");
    std::os::unix::fs::symlink(&script, &link).expect("npm symlink");
    let override_path = temp.path().join("override-kilo");
    fs::write(&override_path, "override fixture").expect("override");
    let environment = BTreeMap::from([
        ("CCCC_OPENCODE_HEADLESS".into(), "1".into()),
        (
            "KILO_BIN_PATH".into(),
            override_path.to_string_lossy().into_owned(),
        ),
    ]);
    let prepared = prepare(
        &[link.to_string_lossy().into_owned()],
        &environment,
        ActorRuntime::Kilo,
    )
    .expect("override");
    assert_eq!(prepared.launch_prefix, [override_path.to_string_lossy()]);
}

/// Exercise the actual Windows process and ConPTY paths, not only argv assembly.
/// This npm-layout fixture needs Node but no Kilo download, credentials or model.
#[cfg(windows)]
#[test]
fn windows_npm_kilo_entrypoint_runs_through_owned_stdio_and_native_terminal() {
    use std::io::{BufRead, BufReader, Write};
    let node = cccc_runtime::resolve_executable_in_path("node.exe", None)
        .expect("install Node for Windows smoke tests");
    let temp = tempfile::tempdir().expect("tempdir");
    let (shim, script) = npm_install(&temp.path().join("npm & tools 空间"), false);
    fs::remove_file(shim.parent().expect("shim directory").join("node.exe"))
        .expect("use installed Node");
    // Model the official launcher's inherited stdio/environment and child tree.
    fs::write(&script, r#"
const worker = `
const fs = require('fs');
fs.writeFileSync(process.env.CCCC_KILO_FIXTURE_OUTPUT, JSON.stringify({args:process.argv.slice(1), value:process.env.KILO_FIXTURE_VALUE}));
process.stdin.on('data', bytes => process.stdout.write(bytes));
process.stdin.resume();`;
require('child_process').spawn(process.execPath, ['-e', worker, ...process.argv.slice(2)], {stdio:'inherit'});
"#).expect("npm launcher fixture");
    let output = temp.path().join("observed.json");
    let path = std::env::join_paths([
        shim.parent().expect("shim directory"),
        node.parent().expect("Node directory"),
    ])
    .expect("fixture PATH");
    let mut env = BTreeMap::from([
        ("PATH".into(), path.to_string_lossy().into_owned()),
        (
            "CCCC_KILO_FIXTURE_OUTPUT".into(),
            output.to_string_lossy().into_owned(),
        ),
        ("KILO_FIXTURE_VALUE".into(), "literal %PATH% & 私有".into()),
    ]);
    let prepared = prepare(&["kilo".into()], &env, ActorRuntime::Kilo)
        .expect("resolve official npm shim from PATH");
    let arguments = ["acp", "--cwd", "C:\\space & 路径", "%PATH%", "quote\"value"];
    let mut command = prepared.launch_prefix.clone();
    command.extend(arguments.map(str::to_owned));
    let (owner, mut stdin, stdout) =
        process::spawn_piped(&command, temp.path(), &env, "kilo-fixture")
            .expect("owned stdio launch");
    let (sender, receiver) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
        let _ = sender.send(result);
    });
    writeln!(stdin, "ACP-回传").expect("stdio input");
    let observed = receiver.recv_timeout(Duration::from_secs(10));
    owner.stop().expect("stop owned npm process tree");
    reader.join().expect("join stdio reader");
    assert_eq!(
        observed
            .expect("bounded output")
            .expect("read output")
            .trim(),
        "ACP-回传"
    );
    let record: Value =
        serde_json::from_slice(&fs::read(&output).expect("worker output")).expect("worker JSON");
    assert_eq!(record["args"], json!(arguments));
    assert_eq!(record["value"], env["KILO_FIXTURE_VALUE"]);
    assert!(!owner.running());

    let output = temp.path().join("terminal.json");
    env.insert(
        "CCCC_KILO_FIXTURE_OUTPUT".into(),
        output.to_string_lossy().into_owned(),
    );
    let mut command = prepared.launch_prefix;
    command.extend([
        "attach".into(),
        "http://127.0.0.1:1234".into(),
        "--session".into(),
        "exact-session".into(),
    ]);
    let group = uuid::Uuid::new_v4().simple().to_string();
    cccc_runtime::start(cccc_runtime::LaunchSpec {
        group_id: group.clone(),
        actor_id: "kilo-fixture".into(),
        runner: cccc_contracts::RunnerKind::Pty,
        command,
        cwd: temp.path().into(),
        env,
        cols: 120,
        rows: 30,
    })
    .expect("native terminal launch");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let record = loop {
        if let Ok(bytes) = fs::read(&output)
            && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
        {
            break Some(value);
        }
        if std::time::Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    cccc_runtime::stop(&group, "kilo-fixture").expect("stop native terminal tree");
    assert_eq!(
        record.expect("native terminal launched")["args"],
        json!([
            "attach",
            "http://127.0.0.1:1234",
            "--session",
            "exact-session"
        ])
    );
}

#[test]
fn native_kilo_resources_preserve_a_nonempty_override() {
    let root = tempfile::tempdir().expect("npm package");
    let resources = root.path().join("tree-sitter");
    std::fs::create_dir(&resources).expect("resources");
    std::fs::write(resources.join("tree-sitter.wasm"), "fixture").expect("wasm");
    let prefix = vec![root.path().join(".kilo").to_string_lossy().into_owned()];
    let mut env = BTreeMap::from([
        ("CCCC_OPENCODE_HEADLESS".into(), "1".into()),
        ("KILO_TREE_SITTER_WASM_DIR".into(), String::new()),
    ]);
    command::configure_native_resources(Path::new(&prefix[0]), &prefix, &mut env);
    assert_eq!(
        env["KILO_TREE_SITTER_WASM_DIR"],
        resources.to_string_lossy()
    );
    env.insert(
        "KILO_TREE_SITTER_WASM_DIR".into(),
        "chosen/resources".into(),
    );
    command::configure_native_resources(Path::new(&prefix[0]), &prefix, &mut env);
    assert_eq!(env["KILO_TREE_SITTER_WASM_DIR"], "chosen/resources");
}

#[test]
fn native_kilo_binary_override_keeps_npm_tree_sitter_resources() {
    let root = tempfile::tempdir().expect("npm package");
    let (shim, script) = npm_install(root.path(), false);
    let resources = script
        .parent()
        .expect("script directory")
        .join("tree-sitter");
    fs::create_dir(&resources).expect("resources");
    fs::write(resources.join("tree-sitter.wasm"), "fixture").expect("wasm");
    let external = root.path().join("custom-native-kilo");
    fs::write(&external, "fixture").expect("native binary");
    let mut env = BTreeMap::from([
        ("CCCC_OPENCODE_HEADLESS".into(), "1".into()),
        (
            "KILO_BIN_PATH".into(),
            external.to_string_lossy().into_owned(),
        ),
    ]);
    let prepared = prepare(
        &[shim.to_string_lossy().into_owned()],
        &env,
        ActorRuntime::Kilo,
    )
    .expect("native override");
    assert_eq!(prepared.launch_prefix, [external.to_string_lossy()]);
    command::configure_native_resources(&prepared.executable, &prepared.launch_prefix, &mut env);
    assert_eq!(env["KILO_TREE_SITTER_WASM_DIR"], canonical(&resources));
}

#[test]
fn flag_unset_keeps_official_kilo_npm_launch_bytes_with_a_native_cache() {
    let temp = tempfile::tempdir().expect("npm install");
    let (shim, script) = npm_install(temp.path(), false);
    fs::write(
        script.parent().expect("bin").join(".kilo"),
        "native fixture",
    )
    .expect("native cache");
    for flag in [None, Some("0")] {
        let mut env = BTreeMap::new();
        if let Some(flag) = flag {
            env.insert("CCCC_OPENCODE_HEADLESS".into(), flag.into());
        }
        let prepared = prepare(
            &[shim.to_string_lossy().into_owned()],
            &env,
            ActorRuntime::Kilo,
        )
        .expect("legacy npm entrypoint");
        assert_eq!(
            prepared.launch_prefix,
            [
                shim.parent()
                    .expect("shim")
                    .join("node.exe")
                    .to_string_lossy(),
                script.to_string_lossy(),
            ]
        );
        let prepared = prepare(
            &[script.to_string_lossy().into_owned()],
            &env,
            ActorRuntime::Kilo,
        )
        .expect("legacy script");
        assert_eq!(prepared.launch_prefix, [script.to_string_lossy()]);
        let cached = script.parent().expect("bin").join(".kilo");
        assert_eq!(
            prepare(
                &[cached.to_string_lossy().into_owned()],
                &env,
                ActorRuntime::Kilo
            )
            .err()
            .expect("legacy executable-name restriction")
            .kind(),
            io::ErrorKind::InvalidInput
        );
        let resources = script.parent().expect("bin").join("tree-sitter");
        fs::create_dir_all(&resources).expect("resources");
        fs::write(resources.join("tree-sitter.wasm"), "fixture").expect("wasm");
        let original_env = env.clone();
        command::configure_native_resources(
            &script,
            &[cached.to_string_lossy().into_owned()],
            &mut env,
        );
        assert_eq!(
            env, original_env,
            "default resource environment must remain unchanged"
        );
    }
}
