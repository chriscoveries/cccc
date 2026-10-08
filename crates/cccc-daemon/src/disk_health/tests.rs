use super::*;
use serde_json::json;

fn fixture(used: u64) -> Snapshot {
    let total = 100 * 1024_u64.pow(3);
    let available = total * (100 - used) / 100;
    Snapshot::from_capacity(
        "fixture-volume".into(),
        total,
        available,
        available,
        Thresholds::default(),
    )
    .expect("valid test fixture")
}
fn home() -> (tempfile::TempDir, HomeLayout) {
    let temp = tempfile::tempdir().expect("valid test fixture");
    let home = HomeLayout::from_path(temp.path().join("home")).expect("valid test fixture");
    home.initialize().expect("valid test fixture");
    (temp, home)
}

#[test]
fn thresholds_include_exact_percent_boundaries_and_strict_nonroot_floor() {
    assert_eq!(fixture(80).severity, Severity::Normal);
    assert_eq!(fixture(84).severity, Severity::Normal);
    assert_eq!(fixture(85).severity, Severity::Warning);
    assert_eq!(fixture(89).severity, Severity::Warning);
    assert_eq!(fixture(90).severity, Severity::Critical);
    assert_eq!(fixture(100).severity, Severity::Critical);
    let gib = 1024_u64.pow(3);
    assert_eq!(
        Snapshot::from_capacity(
            "small".into(),
            20 * gib,
            8 * gib,
            8 * gib,
            Thresholds::default()
        )
        .expect("valid test fixture")
        .severity,
        Severity::Normal
    );
    // Reserved blocks are free but not available to the daemon's non-root user.
    assert_eq!(
        Snapshot::from_capacity(
            "small".into(),
            20 * gib,
            8 * gib,
            8 * gib - 1,
            Thresholds::default()
        )
        .expect("valid test fixture")
        .severity,
        Severity::Warning
    );
    assert!(Snapshot::from_capacity("zero".into(), 0, 0, 0, Thresholds::default()).is_err());
    assert!(Snapshot::from_capacity("invalid".into(), 10, 11, 5, Thresholds::default()).is_err());
}

#[test]
fn one_global_episode_survives_restart_escalates_and_recovers_without_group_fanout() {
    let (_temp, home) = home();
    let mut publisher = Publisher::default();
    publisher
        .publish(&home, fixture(80))
        .expect("valid test fixture");
    publisher
        .publish(&home, fixture(85))
        .expect("valid test fixture");
    publisher
        .publish(&home, fixture(85))
        .expect("valid test fixture");
    Publisher::default()
        .publish(&home, fixture(85))
        .expect("valid test fixture");
    publisher
        .publish(&home, fixture(90))
        .expect("valid test fixture");
    publisher
        .publish(&home, fixture(90))
        .expect("valid test fixture");
    publisher
        .publish(&home, fixture(85))
        .expect("valid test fixture");
    publisher
        .publish(&home, fixture(83))
        .expect("valid test fixture");
    publisher
        .publish(&home, fixture(85))
        .expect("valid test fixture");
    let result = read_events(&home, None, 100).expect("valid test fixture");
    let events = result["events"].as_array().expect("event list");
    assert_eq!(events.len(), 5);
    assert_eq!(events[0]["severity"], "warning");
    assert_eq!(events[1]["severity"], "critical");
    assert_eq!(events[2]["direction"], "down");
    assert_eq!(events[3]["severity"], "normal");
    for event in &events[..4] {
        assert_eq!(event["episode_id"], events[0]["episode_id"]);
    }
    assert_ne!(events[4]["episode_id"], events[0]["episode_id"]);
    assert_eq!(
        std::fs::read_dir(home.groups_dir())
            .expect("valid test fixture")
            .count(),
        0
    );
}

#[test]
fn event_hysteresis_rearms_below_threshold_while_health_remains_current() {
    let (_temp, home) = home();
    let mut publisher = Publisher::default();
    publisher
        .publish(&home, fixture(85))
        .expect("valid test fixture");
    let current = fixture(84);
    assert_eq!(current.severity, Severity::Normal);
    publisher
        .publish(&home, current)
        .expect("valid test fixture");
    publisher
        .publish(&home, fixture(85))
        .expect("valid test fixture");
    assert_eq!(
        read_events(&home, None, 100).expect("valid test fixture")["events"]
            .as_array()
            .expect("valid test fixture")
            .len(),
        1
    );
    publisher
        .publish(&home, fixture(83))
        .expect("valid test fixture");
    publisher
        .publish(&home, fixture(85))
        .expect("valid test fixture");
    assert_eq!(
        read_events(&home, None, 100).expect("valid test fixture")["events"]
            .as_array()
            .expect("valid test fixture")
            .len(),
        3
    );
}

#[test]
fn bounded_reader_paginates_and_explicitly_reports_unknown_or_truncated_cursor() {
    let (_temp, home) = home();
    let mut publisher = Publisher::default();
    for used in [85, 90, 80] {
        publisher
            .publish(&home, fixture(used))
            .expect("valid test fixture");
    }
    let first = read_events(&home, None, 1).expect("valid test fixture");
    assert_eq!(first["events"].as_array().expect("event list").len(), 1);
    assert_eq!(first["has_more"], true);
    let second = read_events(&home, first["cursor"].as_str(), 1).expect("valid test fixture");
    assert_eq!(second["events"][0]["severity"], "critical");
    assert_eq!(second["gap"], false);
    let third = read_events(&home, second["cursor"].as_str(), 1).expect("valid test fixture");
    assert_eq!(third["events"][0]["severity"], "normal");
    assert_eq!(third["has_more"], false);
    let end = read_events(&home, third["cursor"].as_str(), 1).expect("valid test fixture");
    assert_eq!(end["events"], json!([]));
    assert_eq!(end["cursor"], third["cursor"]);
    assert_eq!(
        read_events(&home, Some("lost-id"), 1).expect("valid test fixture")["gap"],
        true
    );
    std::fs::remove_file(home.daemon_dir().join("disk-events.jsonl")).expect("valid test fixture");
    let truncated = read_events(&home, third["cursor"].as_str(), 1).expect("valid test fixture");
    assert_eq!(truncated["gap"], true);
    assert_eq!(truncated["events"], json!([]));
}

#[test]
fn journal_failure_does_not_hide_measured_pressure_or_consume_the_crossing() {
    let (_temp, home) = home();
    let mut settings = cccc_core::settings::load(&home).expect("valid test fixture");
    settings.observability.insert(
        "disk_health".into(),
        json!({"minimum_available_bytes":u64::MAX}),
    );
    cccc_core::settings::save(&home, &settings).expect("valid test fixture");
    let journal = home.daemon_dir().join("disk-events.jsonl");
    std::fs::create_dir(&journal).expect("valid test fixture");
    let mut publisher = Publisher::default();
    assert!(publisher.tick(&home).is_err());
    assert_eq!(health(&home)["severity"], "warning");
    std::fs::remove_dir(&journal).expect("valid test fixture");
    publisher.tick(&home).expect("valid test fixture");
    publisher.tick(&home).expect("valid test fixture");
    assert_eq!(
        read_events(&home, None, 100).expect("valid test fixture")["events"]
            .as_array()
            .expect("valid test fixture")
            .len(),
        1
    );
}

#[test]
fn missing_invalid_configuration_and_corrupt_journal_fail_closed() {
    let (_temp, home) = home();
    let mut settings = cccc_core::settings::load(&home).expect("valid test fixture");
    settings.observability.insert(
        "disk_health".into(),
        json!({"warning_percent":95,"critical_percent":90}),
    );
    cccc_core::settings::save(&home, &settings).expect("valid test fixture");
    assert_eq!(health(&home)["severity"], "unknown");
    assert!(Publisher::default().tick(&home).is_err());
    std::fs::write(home.daemon_dir().join("disk-events.jsonl"), b"{partial")
        .expect("valid test fixture");
    assert!(read_events(&home, None, 1).is_err());
}

#[test]
fn new_home_volume_does_not_inherit_an_old_volumes_latch() {
    let (_temp, home) = home();
    let mut publisher = Publisher::default();
    publisher
        .publish(&home, fixture(90))
        .expect("valid test fixture");
    let mut other = fixture(85);
    other.volume_id = "different-volume".into();
    publisher.publish(&home, other).expect("valid test fixture");
    let events = read_events(&home, None, 100).expect("valid test fixture");
    assert_eq!(events["events"][1]["previous_severity"], "normal");
    assert_ne!(
        events["events"][0]["episode_id"],
        events["events"][1]["episode_id"]
    );
}

#[test]
fn available_floor_recovery_margin_and_disabled_floor_do_not_confuse_rearming() {
    let gib = 1024_u64.pow(3);
    let thresholds = Thresholds::default();
    let low = Snapshot::from_capacity(
        "small".into(),
        20 * gib,
        7 * gib,
        7 * gib,
        thresholds.clone(),
    )
    .expect("valid test fixture");
    assert_eq!(low.event_severity(Severity::Normal), Severity::Warning);
    let recovering = Snapshot::from_capacity(
        "small".into(),
        20 * gib,
        8 * gib,
        8 * gib,
        thresholds.clone(),
    )
    .expect("valid test fixture");
    assert_eq!(recovering.severity, Severity::Normal);
    assert_eq!(
        recovering.event_severity(Severity::Warning),
        Severity::Warning
    );
    let recovered = Snapshot::from_capacity("small".into(), 20 * gib, 9 * gib, 9 * gib, thresholds)
        .expect("valid test fixture");
    assert_eq!(
        recovered.event_severity(Severity::Warning),
        Severity::Normal
    );
    let disabled = Thresholds {
        minimum_available_bytes: 0,
        ..Default::default()
    };
    let small = Snapshot::from_capacity("small".into(), gib, gib / 2, gib / 2, disabled)
        .expect("valid test fixture");
    assert_eq!(small.event_severity(Severity::Warning), Severity::Normal);
}

#[test]
fn percentage_uses_unrounded_df_denominator_and_zero_usable_is_pressure() {
    let thresholds = Thresholds {
        minimum_available_bytes: 0,
        ..Thresholds::default()
    };
    let reserved = Snapshot::from_capacity("reserved".into(), 100, 20, 10, thresholds.clone())
        .expect("valid counters");
    assert!((reserved.used_percent - 100.0 * 80.0 / 90.0).abs() < 0.000001);
    assert_eq!(reserved.severity, Severity::Warning);
    let regular = Snapshot::from_capacity("regular".into(), 100, 20, 20, thresholds.clone())
        .expect("valid counters");
    assert_eq!(regular.used_percent, 80.0);
    let unavailable =
        Snapshot::from_capacity("unavailable".into(), 100, 100, 0, thresholds.clone())
            .expect("valid counters");
    assert_eq!(unavailable.used_percent, 100.0);
    assert_eq!(unavailable.severity, Severity::Critical);
    assert!(Snapshot::from_capacity("invalid".into(), 100, 20, 21, thresholds).is_err());
}

#[test]
fn overlapping_publishers_reconcile_the_durable_latch_before_escalating() {
    let (_temp, home) = home();
    let mut old = Publisher::default();
    let mut replacement = Publisher::default();
    old.publish(&home, fixture(85)).expect("initial warning");
    replacement
        .publish(&home, fixture(85))
        .expect("replacement observes warning");
    replacement
        .publish(&home, fixture(90))
        .expect("replacement escalation");
    old.publish(&home, fixture(90))
        .expect("late old worker tick");
    let records = read_events(&home, None, 100).expect("journal");
    let events = records["events"].as_array().expect("events");
    assert_eq!(
        events.len(),
        2,
        "overlapping workers must not duplicate escalation"
    );
    assert_eq!(events[0]["episode_id"], events[1]["episode_id"]);
}
