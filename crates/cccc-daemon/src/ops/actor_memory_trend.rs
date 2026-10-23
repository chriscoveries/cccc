//! Harness RAM over the last 72 hours.
//!
//! The per-Actor reading in [`super::actor_memory`] answers "what does this Actor
//! hold right now". This module answers the other question an operator asks when
//! planning: what does a harness *cost*, in RAM, across the Actors running it.
//!
//! One sample per Actor per cadence is folded into hourly buckets keyed by
//! `(harness, mode)`, so a 72-hour trend is bounded by construction: each bucket
//! keeps the hour's peak, the sum and the count, and the oldest hour drops off as
//! a new one opens. Nothing here is read by delivery, idling, health or dispatch;
//! the figures are display only, like the row fields they are built from.

use cccc_contracts::Actor;
use cccc_core::{GroupDoc, GroupStore, HomeLayout};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};

/// Hours kept per key.
pub(crate) const WINDOW_HOURS: i64 = 72;

const HOUR_SECONDS: i64 = 3600;
const STATE_FILE: &str = "harness-ram-72h.json";
const SCHEMA: u32 = 1;

/// One hour of samples for one key, in MiB — the same unit as the row fields
/// this is folded from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct Bucket {
    peak_rss_mib: u64,
    sum_rss_mib: u64,
    rss_samples: u32,
    peak_pss_mib: Option<u64>,
    sum_pss_mib: u64,
    pss_samples: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct HourEntry {
    hour: i64,
    #[serde(flatten)]
    bucket: Bucket,
}

/// One `(harness, mode)` series: its hours, oldest first, at most [`WINDOW_HOURS`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Series {
    harness: String,
    mode: String,
    #[serde(default)]
    hours: Vec<HourEntry>,
}

/// The whole trend for one Group, as persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Trend {
    #[serde(default = "schema")]
    schema: u32,
    #[serde(default)]
    series: Vec<Series>,
}

const fn schema() -> u32 {
    SCHEMA
}

impl Default for Trend {
    fn default() -> Self {
        Self {
            schema: SCHEMA,
            series: Vec::new(),
        }
    }
}

struct GroupTrend {
    trend: Trend,
    loaded: bool,
    /// Samples recorded since the last successful write. A restart must not
    /// lose a window that a report has already shown, so the file is rewritten
    /// at the end of each sampling tick rather than on a timer.
    dirty: bool,
    /// Where this Group's window lives. Resolved once: a Group's state
    /// directory does not move under a running daemon.
    path: Option<PathBuf>,
}

static GROUPS: LazyLock<Mutex<BTreeMap<String, GroupTrend>>> = LazyLock::new(Mutex::default);

fn groups() -> MutexGuard<'static, BTreeMap<String, GroupTrend>> {
    GROUPS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The mode the daemon resolves for an Actor, as the trend keys it: the runner
/// the Actor is actually on, plus `-acp` when it runs the ACP mode. An Actor
/// the managed supervisor does not know is a PTY session, which is the only
/// other way a pid reaches this sampler.
fn actor_mode(group_id: &str, actor_id: &str) -> String {
    let resolved = super::local_headless::structured_state(group_id, actor_id);
    let field = |name: &str, fallback: &str| {
        resolved
            .as_ref()
            .and_then(|state| state.get(name))
            .and_then(Value::as_str)
            .unwrap_or(fallback)
            .to_owned()
    };
    let runner = field("runner", "pty");
    if field("runtime_mode", "default") == "acp" {
        format!("{runner}-acp")
    } else {
        runner
    }
}

/// The harness id as the Actor row spells it.
fn harness_id(runtime: cccc_contracts::ActorRuntime) -> String {
    serde_json::to_value(runtime)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Fold one Actor's reading into its Group's trend. The state file is written
/// by [`flush_group`] at the end of the sampling tick, so a daemon restart
/// cannot drop samples that have already been reported.
pub(crate) fn record_actor(
    home: &HomeLayout,
    group: &GroupDoc,
    actor: &Actor,
    epoch: i64,
    rss_mib: u64,
    pss_mib: Option<u64>,
) {
    let harness = harness_id(actor.runtime);
    let mode = actor_mode(&group.group_id, &actor.id);
    record(
        home,
        &group.group_id,
        &harness,
        &mode,
        epoch,
        Some(rss_mib),
        pss_mib,
    );
}

fn record(
    home: &HomeLayout,
    group_id: &str,
    harness: &str,
    mode: &str,
    epoch: i64,
    rss_mib: Option<u64>,
    pss_mib: Option<u64>,
) {
    let mut groups = groups();
    let entry = groups
        .entry(group_id.to_owned())
        .or_insert_with(|| GroupTrend::new(state_path(home, group_id).ok()));
    let path = entry.path.clone();
    load_once(entry, path.as_deref());
    entry.trend.record(harness, mode, epoch, rss_mib, pss_mib);
    entry.trend.prune(epoch);
    entry.dirty = true;
}

/// Write a Group's window once the tick that sampled it has finished.
///
/// The window lives in memory between ticks and one tick's samples arrive one
/// Actor at a time, so writing there would rewrite the file once per Actor.
/// Writing at the tick boundary keeps it to one atomic replace per tick and,
/// unlike a wall-clock throttle, leaves nothing on the floor if the daemon
/// restarts a moment later.
pub(crate) fn flush_group(home: &HomeLayout, group_id: &str) {
    let mut groups = groups();
    let entry = groups
        .entry(group_id.to_owned())
        .or_insert_with(|| GroupTrend::new(state_path(home, group_id).ok()));
    let path = entry.path.clone();
    load_once(entry, path.as_deref());
    flush(entry, path.as_deref());
}

fn flush(entry: &mut GroupTrend, path: Option<&Path>) {
    if !entry.dirty {
        return;
    }
    let Some(path) = path else {
        // No state directory to write to: keep the window in memory rather
        // than clearing the flag and pretending it was saved.
        return;
    };
    if entry.trend.save(path).is_ok() {
        entry.dirty = false;
    }
}

/// The 72-hour report for one Group.
pub(crate) fn report(home: &HomeLayout, group_id: &str, now: i64) -> Value {
    let mut groups = groups();
    let entry = groups
        .entry(group_id.to_owned())
        .or_insert_with(|| GroupTrend::new(state_path(home, group_id).ok()));
    let path = entry.path.clone();
    load_once(entry, path.as_deref());
    entry.trend.prune(now);
    // A figure a caller has been shown must survive a restart a moment later.
    flush(entry, path.as_deref());
    entry.trend.report(now)
}

/// Drop the trend of Groups that no longer exist.
pub(crate) fn retain_groups(known: &std::collections::BTreeSet<String>) {
    groups().retain(|group, _| known.contains(group));
}

impl GroupTrend {
    fn new(path: Option<PathBuf>) -> Self {
        Self {
            trend: Trend::default(),
            loaded: false,
            dirty: false,
            path,
        }
    }
}

fn load_once(entry: &mut GroupTrend, path: Option<&Path>) {
    if entry.loaded {
        return;
    }
    entry.loaded = true;
    // An absent, unreadable or corrupt file is an empty window, not an error:
    // the figures are a diagnostic and the next samples rebuild them.
    if let Some(path) = path {
        entry.trend = Trend::load(path);
    }
}

fn state_path(home: &HomeLayout, group_id: &str) -> io::Result<PathBuf> {
    Ok(GroupStore::new(home.clone())?
        .state_dir(group_id)?
        .join(STATE_FILE))
}

fn hour_of(epoch: i64) -> i64 {
    epoch.div_euclid(HOUR_SECONDS)
}

fn hour_label(hour: i64) -> String {
    DateTime::from_timestamp(hour * HOUR_SECONDS, 0)
        .map(|at: DateTime<Utc>| at.to_rfc3339_opts(SecondsFormat::Secs, true))
        .unwrap_or_default()
}

fn number(value: Option<u64>) -> Value {
    value.map_or(Value::Null, Value::from)
}

impl Trend {
    fn series_mut(&mut self, harness: &str, mode: &str) -> &mut Series {
        let at = self
            .series
            .iter()
            .position(|series| series.harness == harness && series.mode == mode);
        let at = match at {
            Some(at) => at,
            None => {
                self.series.push(Series {
                    harness: harness.to_owned(),
                    mode: mode.to_owned(),
                    hours: Vec::new(),
                });
                self.series.len() - 1
            }
        };
        &mut self.series[at]
    }

    fn record(
        &mut self,
        harness: &str,
        mode: &str,
        epoch: i64,
        rss_mib: Option<u64>,
        pss_mib: Option<u64>,
    ) {
        let hour = hour_of(epoch);
        let series = self.series_mut(harness, mode);
        let at = series
            .hours
            .binary_search_by_key(&hour, |entry| entry.hour)
            .unwrap_or_else(|at| at);
        if at == series.hours.len() || series.hours[at].hour != hour {
            series.hours.insert(
                at,
                HourEntry {
                    hour,
                    bucket: Bucket::default(),
                },
            );
        }
        let bucket = &mut series.hours[at].bucket;
        if let Some(rss) = rss_mib {
            bucket.peak_rss_mib = bucket.peak_rss_mib.max(rss);
            bucket.sum_rss_mib = bucket.sum_rss_mib.saturating_add(rss);
            bucket.rss_samples = bucket.rss_samples.saturating_add(1);
        }
        if let Some(pss) = pss_mib {
            bucket.peak_pss_mib = Some(bucket.peak_pss_mib.map_or(pss, |peak| peak.max(pss)));
            bucket.sum_pss_mib = bucket.sum_pss_mib.saturating_add(pss);
            bucket.pss_samples = bucket.pss_samples.saturating_add(1);
        }
    }

    /// Keep the window: hours older than the one `WINDOW_HOURS` back are gone,
    /// and a series with no hours left is gone with them.
    fn prune(&mut self, now: i64) {
        let oldest = hour_of(now) - (WINDOW_HOURS - 1);
        for series in &mut self.series {
            series.hours.retain(|entry| entry.hour >= oldest);
        }
        self.series.retain(|series| !series.hours.is_empty());
    }

    fn report(&self, now: i64) -> Value {
        let oldest = hour_of(now) - (WINDOW_HOURS - 1);
        let mut keys = Vec::new();
        for series in &self.series {
            let hours = series
                .hours
                .iter()
                .filter(|entry| entry.hour >= oldest)
                .collect::<Vec<_>>();
            if hours.is_empty() {
                continue;
            }
            let rss_samples = hours
                .iter()
                .map(|entry| u64::from(entry.bucket.rss_samples))
                .sum::<u64>();
            let rss_sum = hours
                .iter()
                .map(|entry| entry.bucket.sum_rss_mib)
                .sum::<u64>();
            let rss_peak = hours
                .iter()
                .map(|entry| entry.bucket.peak_rss_mib)
                .max()
                .unwrap_or_default();
            let pss_samples = hours
                .iter()
                .map(|entry| u64::from(entry.bucket.pss_samples))
                .sum::<u64>();
            let pss_sum = hours
                .iter()
                .map(|entry| entry.bucket.sum_pss_mib)
                .sum::<u64>();
            let pss_peak = hours
                .iter()
                .filter_map(|entry| entry.bucket.peak_pss_mib)
                .max();
            let mut row = Map::new();
            row.insert("harness".into(), Value::from(series.harness.clone()));
            row.insert("mode".into(), Value::from(series.mode.clone()));
            row.insert(
                "peak_rss_mib".into(),
                number((rss_samples > 0).then_some(rss_peak)),
            );
            row.insert(
                "avg_rss_mib".into(),
                number((rss_samples > 0).then(|| rss_sum / rss_samples.max(1))),
            );
            row.insert("samples".into(), Value::from(rss_samples));
            row.insert("peak_pss_mib".into(), number(pss_peak));
            row.insert(
                "avg_pss_mib".into(),
                number((pss_samples > 0).then(|| pss_sum / pss_samples.max(1))),
            );
            row.insert("pss_samples".into(), Value::from(pss_samples));
            row.insert("hours_observed".into(), Value::from(hours.len()));
            let series_rows = hours
                .iter()
                .map(|entry| {
                    let samples = u64::from(entry.bucket.rss_samples);
                    json!({
                        "hour": hour_label(entry.hour),
                        "peak_rss_mib": number((samples > 0).then_some(entry.bucket.peak_rss_mib)),
                        "avg_rss_mib": number((samples > 0).then(|| entry.bucket.sum_rss_mib / samples.max(1))),
                        "samples": samples,
                    })
                })
                .collect::<Vec<_>>();
            row.insert("series".into(), Value::Array(series_rows));
            keys.push(Value::Object(row));
        }
        // Busiest first: the harness holding most of the host is the one an
        // operator is looking for.
        keys.sort_by_key(|value| {
            std::cmp::Reverse(
                value
                    .get("peak_rss_mib")
                    .and_then(Value::as_u64)
                    .or_else(|| value.get("peak_pss_mib").and_then(Value::as_u64))
                    .unwrap_or_default(),
            )
        });
        json!({
            "window_hours": WINDOW_HOURS,
            "bucket_seconds": HOUR_SECONDS,
            "as_of": hour_label(hour_of(now)),
            "keys": keys,
        })
    }

    fn load(path: &Path) -> Self {
        let Ok(text) = fs::read_to_string(path) else {
            return Self::default();
        };
        match serde_json::from_str::<Self>(&text) {
            Ok(trend) if trend.schema == SCHEMA => trend,
            _ => Self::default(),
        }
    }

    /// Replace the file wholesale, by rename, so a reader never sees a half
    /// written window and a crash mid-write leaves the previous one intact.
    fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("json.tmp");
        fs::write(
            &temporary,
            serde_json::to_vec(self).map_err(io::Error::other)?,
        )?;
        fs::rename(&temporary, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The captured census stream: one line per real census tick, each holding
    /// the per-harness PSS the host's own census recorded at that tick.
    fn captured_stream() -> Vec<(i64, Vec<(String, u64)>)> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/actor-memory-census/stream.jsonl");
        let text = std::fs::read_to_string(&path).expect("captured census stream");
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let capture: Value = serde_json::from_str(line).expect("capture line");
                let at = capture["captured_at"].as_str().expect("captured_at");
                let at = DateTime::parse_from_rfc3339(at)
                    .expect("captured_at is rfc3339")
                    .timestamp();
                let rows = capture["rows"]
                    .as_array()
                    .expect("rows")
                    .iter()
                    .map(|row| {
                        (
                            row["harness"].as_str().expect("harness").to_owned(),
                            row["pss_kb"].as_u64().expect("pss_kb"),
                        )
                    })
                    .collect::<Vec<_>>();
                (at, rows)
            })
            .collect()
    }

    fn kib_to_mib(kib: u64) -> u64 {
        kib.saturating_add(512) / 1024
    }

    #[test]
    fn folds_the_captured_census_stream_into_peaks_and_averages() {
        let stream = captured_stream();
        assert!(
            stream.len() >= 3,
            "the fixture holds several real ticks: {}",
            stream.len()
        );
        let mut trend = Trend::default();
        for (at, rows) in &stream {
            for (harness, pss_kb) in rows {
                trend.record(harness, "headless", *at, None, Some(kib_to_mib(*pss_kb)));
            }
        }
        let now = stream.iter().map(|(at, _)| *at).max().expect("a tick");
        trend.prune(now);

        // The harness the capture saw most often, so the expectation is built
        // from repeated real readings of the same harness rather than one.
        let mut counts = BTreeMap::new();
        for (_, rows) in &stream {
            for (harness, _) in rows {
                *counts.entry(harness.clone()).or_insert(0usize) += 1;
            }
        }
        let busiest = counts
            .iter()
            .max_by_key(|(_, count)| **count)
            .map(|(harness, _)| harness.clone())
            .expect("a harness");
        let observed = stream
            .iter()
            .flat_map(|(_, rows)| {
                rows.iter()
                    .filter(|(harness, _)| harness == &busiest)
                    .map(|(_, pss_kb)| kib_to_mib(*pss_kb))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        assert!(observed.len() >= 3, "several readings of {busiest}");

        let report = trend.report(now);
        let rows = report["keys"].as_array().expect("keys");
        let row = rows
            .iter()
            .find(|row| row["harness"].as_str() == Some(busiest.as_str()))
            .unwrap_or_else(|| panic!("{busiest} is in the report"));

        assert_eq!(row["mode"], "headless");
        assert_eq!(
            row["pss_samples"].as_u64(),
            Some(observed.len() as u64),
            "every census reading of {busiest} is counted"
        );
        assert_eq!(
            row["samples"], 0,
            "the census records PSS, not RSS, so no RSS sample was taken"
        );
        assert_eq!(
            row["peak_pss_mib"].as_u64(),
            observed.iter().copied().max(),
            "peak is the largest real reading of {busiest}"
        );
        let sum = observed.iter().sum::<u64>();
        assert_eq!(
            row["avg_pss_mib"].as_u64(),
            Some(sum / observed.len() as u64),
            "average is the mean of the same real readings"
        );
        assert_eq!(
            row["peak_rss_mib"],
            Value::Null,
            "the census records PSS, not RSS"
        );
        let hours = row["series"].as_array().expect("series");
        assert_eq!(
            hours.len(),
            row["hours_observed"].as_u64().unwrap_or_default() as usize
        );
        assert!(
            !hours.is_empty() && hours.len() <= WINDOW_HOURS as usize,
            "the series is bounded by the window"
        );
        assert_eq!(report["window_hours"].as_i64(), Some(WINDOW_HOURS));
        // Busiest first: the first row is the largest peak of the capture.
        let first_peak = rows[0]["peak_pss_mib"].as_u64().unwrap_or_default();
        assert!(
            rows.iter()
                .all(|row| row["peak_pss_mib"].as_u64().unwrap_or_default() <= first_peak),
            "rows are ordered by peak"
        );
    }

    #[test]
    fn a_real_tree_carries_rss_and_pss_together() {
        // The captured `/proc` tree: 336 MiB resident, 136 MiB proportional.
        let mut trend = Trend::default();
        trend.record("codex", "pty", 1_700_000_000, Some(336), Some(136));
        let report = trend.report(1_700_000_000);
        let row = &report["keys"][0];
        assert_eq!(row["peak_rss_mib"].as_u64(), Some(336));
        assert_eq!(row["avg_rss_mib"].as_u64(), Some(336));
        assert_eq!(row["peak_pss_mib"].as_u64(), Some(136));
        assert_eq!(row["avg_pss_mib"].as_u64(), Some(136));
        assert_eq!(row["samples"].as_u64(), Some(1));
    }

    #[test]
    fn averages_weight_every_sample_not_every_hour() {
        let mut trend = Trend::default();
        let hour = 1_700_000_000 - (1_700_000_000 % HOUR_SECONDS);
        // Three cheap samples in the first hour, one expensive in the second.
        for offset in [0, 60, 120] {
            trend.record("claude", "pty", hour + offset, Some(100), None);
        }
        trend.record("claude", "pty", hour + HOUR_SECONDS, Some(400), None);
        let report = trend.report(hour + HOUR_SECONDS);
        let row = &report["keys"][0];
        assert_eq!(row["samples"].as_u64(), Some(4));
        assert_eq!(row["peak_rss_mib"].as_u64(), Some(400));
        assert_eq!(
            row["avg_rss_mib"].as_u64(),
            Some(175),
            "(100+100+100+400)/4, not the mean of the two hourly averages"
        );
        assert_eq!(row["hours_observed"].as_u64(), Some(2));
    }

    #[test]
    fn keeps_seventy_two_hours_and_drops_the_oldest() {
        let mut trend = Trend::default();
        let start = 1_700_000_000 - (1_700_000_000 % HOUR_SECONDS);
        for hour in 0..75 {
            trend.record(
                "hermes",
                "pty",
                start + hour * HOUR_SECONDS,
                Some(hour as u64),
                None,
            );
        }
        let now = start + 74 * HOUR_SECONDS;
        trend.prune(now);
        let report = trend.report(now);
        let row = &report["keys"][0];
        assert_eq!(row["hours_observed"].as_u64(), Some(WINDOW_HOURS as u64));
        assert_eq!(
            row["samples"].as_u64(),
            Some(WINDOW_HOURS as u64),
            "the three oldest hours are gone"
        );
        let series = row["series"].as_array().expect("series");
        assert_eq!(series.len(), WINDOW_HOURS as usize);
        assert_eq!(
            series[0]["avg_rss_mib"].as_u64(),
            Some(3),
            "the window starts three hours in"
        );
        // The report drops old hours from the window on its own, so a prune that
        // never happens would still read correctly and only grow the store. The
        // store itself is what must stay bounded.
        assert_eq!(
            trend.series[0].hours.len(),
            WINDOW_HOURS as usize,
            "seventy-five recorded hours leave seventy-two kept, not seventy-five retained"
        );
    }

    #[test]
    fn survives_a_restart_through_the_state_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("state").join(STATE_FILE);
        let mut trend = Trend::default();
        trend.record("devin", "headless-acp", 1_700_000_000, Some(512), Some(300));
        trend.record("devin", "headless-acp", 1_700_000_600, Some(600), Some(310));
        trend.save(&path).expect("save");

        let reloaded = Trend::load(&path);
        assert_eq!(reloaded, trend);
        let before = trend.report(1_700_000_600);
        let after = reloaded.report(1_700_000_600);
        assert_eq!(before, after);
        assert_eq!(after["keys"][0]["peak_rss_mib"].as_u64(), Some(600));
        assert_eq!(after["keys"][0]["mode"], "headless-acp");
    }

    #[test]
    fn a_corrupt_state_file_starts_an_empty_window() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(STATE_FILE);
        std::fs::write(&path, b"{ not json").expect("write");
        assert_eq!(Trend::load(&path), Trend::default());
        std::fs::write(&path, br#"{"schema":99,"series":[]}"#).expect("write");
        assert_eq!(
            Trend::load(&path),
            Trend::default(),
            "a file from another schema is not read as this one"
        );
    }

    /// A Group home the sampler can address without a daemon around it.
    fn test_home() -> (tempfile::TempDir, HomeLayout) {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path()).expect("home");
        (temp, home)
    }

    fn state_file(temp: &tempfile::TempDir, group_id: &str) -> PathBuf {
        temp.path()
            .join("groups")
            .join(group_id)
            .join("state")
            .join(STATE_FILE)
    }

    /// The stage restart showed a report of 189 samples becoming 174, because
    /// the state file was rewritten on a wall-clock timer: the samples recorded
    /// since the last write were still only in memory. A tick that has finished
    /// must be on disk, so a restart a moment later shows the same window.
    #[test]
    fn a_finished_tick_is_on_disk_before_the_next_one() {
        let (temp, home) = test_home();
        let group = "g_tick_on_disk";
        let now = 1_700_000_000;
        // Twelve samples ten seconds apart: one stage tick's worth of readings
        // for a single (harness, mode) key, all inside the current hour.
        for sample in 0..12 {
            record(
                &home,
                group,
                "hermes",
                "pty",
                now + sample * 10,
                Some(500 + sample as u64),
                Some(300 + sample as u64),
            );
        }
        flush_group(&home, group);

        let shown = report(&home, group, now + 120);
        assert_eq!(shown["keys"][0]["samples"].as_u64(), Some(12));

        // A restart: nothing in memory survives, only the file.
        let reloaded = Trend::load(&state_file(&temp, group));
        assert_eq!(
            reloaded.report(now + 120),
            shown,
            "a restart right after the tick must not drop samples the report already showed"
        );
        assert_eq!(
            reloaded.report(now + 120)["keys"][0]["series"][0]["samples"].as_u64(),
            Some(12),
            "the current hour keeps every sample, not only those written by the last save"
        );
    }

    /// The same restart through the report path alone: reading the window is
    /// enough to make what was read durable, so a caller that samples and then
    /// restarts does not lose the reading it was given.
    #[test]
    fn what_a_report_showed_survives_the_restart_that_follows() {
        let (temp, home) = test_home();
        let group = "g_report_survives";
        let now = 1_700_000_000;
        record(&home, group, "devin", "headless", now, Some(700), None);
        record(&home, group, "devin", "headless", now + 10, Some(900), None);

        let shown = report(&home, group, now + 20);
        let reloaded = Trend::load(&state_file(&temp, group));
        assert_eq!(reloaded.report(now + 20), shown);
        assert_eq!(
            reloaded.report(now + 20)["keys"][0]["peak_rss_mib"].as_u64(),
            Some(900)
        );
    }

    #[test]
    fn a_key_with_no_samples_is_absent_not_zero() {
        let trend = Trend::default();
        let report = trend.report(1_700_000_000);
        assert_eq!(report["keys"].as_array().map(Vec::len), Some(0));
    }
}
