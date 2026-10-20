#![cfg(unix)]

use cccc_contracts::RunnerKind;
use cccc_runtime::{LaunchSpec, RuntimeSessionProbe};
use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn spec(group_id: &str, actor_id: &str, env: BTreeMap<String, String>) -> LaunchSpec {
    LaunchSpec {
        group_id: group_id.into(),
        actor_id: actor_id.into(),
        runner: RunnerKind::Pty,
        command: vec!["sh".into(), "-c".into(), "sleep 30".into()],
        cwd: std::env::temp_dir(),
        env,
        cols: 80,
        rows: 24,
    }
}

fn identity_env(group_id: &str, actor_id: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("CCCC_GROUP_ID".to_owned(), group_id.to_owned()),
        ("CCCC_ACTOR_ID".to_owned(), actor_id.to_owned()),
    ])
}

fn probe<'a>(probes: &'a [RuntimeSessionProbe], actor_id: &str) -> &'a RuntimeSessionProbe {
    probes
        .iter()
        .find(|probe| probe.actor_id == actor_id)
        .expect("registered session")
}

fn wait_for_identity(actor_id: &str) -> Vec<RuntimeSessionProbe> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let probes = cccc_runtime::probe_sessions();
        let found = probes
            .iter()
            .find(|probe| probe.actor_id == actor_id)
            .and_then(|probe| probe.identity.as_ref());
        if found.is_some() || Instant::now() > deadline {
            return probes;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn the_probe_reports_the_identity_each_process_carries() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let group_id = format!("g_probe_{}_{}", std::process::id(), unique);
    let healthy = "probe-healthy";
    let spoofed = "probe-spoofed";

    cccc_runtime::start(spec(&group_id, healthy, identity_env(&group_id, healthy)))
        .expect("start healthy session");
    // The start path stamps the real identity over anything the spec carries:
    // an actor cannot be given another actor's identity through LaunchSpec.env.
    // (A process that rewrites its own environment after exec is the vector the
    // doctor sweep exists for, and that needs a self-rewriting command, not this
    // one. See T831's scratch receipt for the real crossed run.)
    cccc_runtime::start(spec(
        &group_id,
        spoofed,
        identity_env(&group_id, healthy),
    ))
    .expect("start spoofed session");

    let probes = wait_for_identity(healthy);
    let healthy_probe = probe(&probes, healthy);
    assert!(healthy_probe.running, "healthy session should be running");
    assert!(
        healthy_probe.process_alive,
        "healthy process should exist"
    );
    assert!(
        healthy_probe.pid.is_some(),
        "healthy session should report a pid"
    );
    assert!(!healthy_probe.started_at.is_empty());
    let identity = healthy_probe.identity.as_ref().expect("healthy identity");
    assert_eq!(identity.group_id, group_id);
    assert_eq!(identity.actor_id, healthy);

    let spoofed_probe = probe(&probes, spoofed);
    let spoofed_identity = spoofed_probe.identity.as_ref().expect("spoofed identity");
    assert_eq!(
        spoofed_identity.actor_id, spoofed,
        "LaunchSpec.env must not be able to give a session another actor's identity"
    );
    assert_eq!(spoofed_identity.group_id, group_id);

    // Probes are ordered by (group, actor) so two runs of doctor diff cleanly.
    let ordered = probes
        .iter()
        .map(|probe| (probe.group_id.clone(), probe.actor_id.clone()))
        .collect::<Vec<_>>();
    let mut sorted = ordered.clone();
    sorted.sort();
    assert_eq!(ordered, sorted);

    cccc_runtime::stop(&group_id, healthy).expect("stop healthy");
    cccc_runtime::stop(&group_id, spoofed).expect("stop spoofed");
}
