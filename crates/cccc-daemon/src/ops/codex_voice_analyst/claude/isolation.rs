use std::collections::BTreeMap;
use std::io;
use tokio::process::Command;

/// Do not use OwnedProcessTree here: the provider supervisor must outlive the
/// daemon. Only the short-lived background launcher is owned by this Command.
pub(super) async fn command(
    executable: &str,
    arguments: &[String],
    environment: &BTreeMap<String, String>,
    detach: bool,
) -> io::Result<Command> {
    let mut command = super::command::process_command(executable, arguments, environment)?;
    if !detach {
        return Ok(command);
    }
    if cfg!(windows) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Claude detach is unsupported under the Windows daemon job object",
        ));
    }

    #[cfg(target_os = "linux")]
    if std::path::Path::new("/run/systemd/system").exists() {
        if let Some(runner) = user_scope_runner(environment).await {
            command = Command::new(runner);
            command
                .args([
                    "--user",
                    "--scope",
                    "--quiet",
                    "--collect",
                    "--expand-environment=no",
                    "--",
                ])
                .arg(executable)
                .args(arguments);
        } else {
            tracing::warn!(
                "Claude detach has no systemd user scope; service deployments require KillMode=process and a separate OOM boundary"
            );
        }
    }
    #[cfg(unix)]
    command.process_group(0);
    Ok(command)
}

#[cfg(target_os = "linux")]
async fn user_scope_runner(environment: &BTreeMap<String, String>) -> Option<std::path::PathBuf> {
    let runner = crate::ops::codex_voice_analyst::launch_command::resolve_runtime_executable(
        "systemd-run",
        environment,
    )
    .ok()?;
    // Probe only a no-op. Never fall back after a provider launch: even a
    // nonzero launcher exit can leave a job behind, making a retry unsafe.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let result = tokio::time::timeout_at(
            deadline,
            Command::new(&runner)
                .args([
                    "--user",
                    "--scope",
                    "--quiet",
                    "--collect",
                    "--expand-environment=no",
                    "--",
                    "/bin/true",
                ])
                .env_clear()
                .envs(environment)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .status(),
        )
        .await;
        match result {
            Ok(Ok(status)) => return status.success().then_some(runner),
            Ok(Err(error))
                if error.raw_os_error() == Some(nix::errno::Errno::ETXTBSY as i32)
                    || error.kind() == io::ErrorKind::Interrupted =>
            {
                // Another concurrent fork can briefly inherit a writable
                // descriptor to the executable. Only retry this no-op probe,
                // within its original deadline, never the provider launch.
                if tokio::time::Instant::now() >= deadline {
                    return None;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            _ => return None,
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn background_launcher_has_its_own_process_group() {
        let mut command = command(
            "/bin/sh",
            &["-c".into(), "ps -o pgid= -p $$".into()],
            &BTreeMap::new(),
            true,
        )
        .await
        .expect("isolated command");
        command.stdout(std::process::Stdio::piped());
        let child = command.spawn().expect("launcher");
        let pid = child.id().expect("pid");
        let output = child.wait_with_output().await.expect("output");
        assert!(output.status.success());
        let group: u32 = String::from_utf8(output.stdout)
            .expect("utf8")
            .trim()
            .parse()
            .expect("group");
        assert_eq!(group, pid);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod reviewer_isolation_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn reviewer_default_process_group_is_inherited() {
        let mut command = command(
            "/bin/sh",
            &[
                "-c".into(),
                "ps -o pgid= -p $$; ps -o pgid= -p $PPID".into(),
            ],
            &BTreeMap::new(),
            false,
        )
        .await
        .expect("command");
        command.stdout(std::process::Stdio::piped());
        let output = command.output().await.expect("output");
        assert!(output.status.success());
        let output = String::from_utf8(output.stdout).expect("utf8");
        let groups: Vec<&str> = output.split_whitespace().collect();
        assert_eq!(groups[0], groups[1]);
    }

    #[tokio::test]
    async fn reviewer_scope_probe_success_failure_timeout() {
        let temp = tempfile::tempdir().expect("scratch runner");
        let runner = temp.path().join("systemd-run");
        let log = temp.path().join("args");
        let environment = BTreeMap::from([
            ("PATH".into(), temp.path().to_string_lossy().into_owned()),
            ("REVIEW_LOG".into(), log.to_string_lossy().into_owned()),
        ]);
        for (body, success) in [
            ("printf '%s\\n' \"$@\" > \"$REVIEW_LOG\"\nexit 0\n", true),
            ("exit 1\n", false),
            ("exec /bin/sleep 10\n", false),
        ] {
            std::fs::write(&runner, format!("#!/bin/sh\n{body}")).expect("runner");
            std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o755))
                .expect("permissions");
            let start = std::time::Instant::now();
            assert_eq!(user_scope_runner(&environment).await.is_some(), success);
            assert!(start.elapsed() < std::time::Duration::from_secs(3));
            if success {
                assert_eq!(
                    std::fs::read_to_string(&log).expect("probe args"),
                    "--user\n--scope\n--quiet\n--collect\n--expand-environment=no\n--\n/bin/true\n"
                );
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod literal_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn scope_preserves_literal_dollars_and_rejects_unsupported_runner() {
        let temp = tempfile::tempdir().expect("private runner");
        let runner = temp.path().join("systemd-run");
        let env = BTreeMap::from([
            ("PATH".into(), temp.path().to_string_lossy().into_owned()),
            ("HOME".into(), "expanded-value".into()),
        ]);
        std::fs::write(&runner, "#!/bin/sh\nfor arg do if [ \"$arg\" = --expand-environment=no ]; then exit 1; fi; done\nexit 0\n").expect("unsupported runner");
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o755))
            .expect("permissions");
        assert!(user_scope_runner(&env).await.is_none());
        std::fs::write(
            &runner,
            "#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nexec \"$@\"\n",
        )
        .expect("literal runner");
        let mut child = command(
            "/bin/printf",
            &[
                "%s\\n".into(),
                "${HOME}".into(),
                "/tmp/$HOME/settings.json".into(),
            ],
            &env,
            true,
        )
        .await
        .expect("scope command");
        child
            .env_clear()
            .envs(&env)
            .stdout(std::process::Stdio::piped());
        let output = child.output().await.expect("literal argv");
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).expect("output"),
            "${HOME}\n/tmp/$HOME/settings.json\n"
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod executable_busy_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn scope_probe_retries_executable_busy_within_its_deadline() {
        let temp = tempfile::tempdir().expect("private probe");
        let runner = temp.path().join("systemd-run");
        std::fs::write(&runner, "#!/bin/sh\nexit 0\n").expect("runner");
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o755))
            .expect("permissions");
        let environment =
            BTreeMap::from([("PATH".into(), temp.path().to_string_lossy().into_owned())]);
        let writer = std::fs::OpenOptions::new()
            .write(true)
            .open(&runner)
            .expect("writer");
        let release = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            drop(writer);
        });
        assert!(
            user_scope_runner(&environment).await.is_some(),
            "retry transient ETXTBSY rather than falling back"
        );
        release.await.expect("released writer");
        let _writer = std::fs::OpenOptions::new()
            .write(true)
            .open(&runner)
            .expect("writer");
        let start = std::time::Instant::now();
        assert!(user_scope_runner(&environment).await.is_none());
        assert!(start.elapsed() < std::time::Duration::from_secs(3));
    }
}
