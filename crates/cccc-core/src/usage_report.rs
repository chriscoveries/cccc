//! Bounded, content-free projection of observed task and usage events.
//!
//! Context commits and ledger writes are not atomic. Observed subtotals never
//! prove complete accounting, including when the bounded reader reaches EOF.
use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::usage_price::PriceTable;

#[derive(Default)]
struct Task {
    state: Option<Value>,
    active: Option<DateTime<Utc>>,
    intervals: Vec<(DateTime<Utc>, DateTime<Utc>)>,
    reopened: u64,
    blockers: u64,
    invalid: bool,
}

#[derive(Default)]
struct Cohort {
    tasks: BTreeSet<String>,
    turns: u64,
    tokens: [u128; 4],
    priced: u128,
    priced_turns: u64,
    unpriced_turns: u64,
    unknown_membership: u64,
}

fn time(value: &Value) -> Option<DateTime<Utc>> {
    let parsed = DateTime::parse_from_rfc3339(value.as_str()?).ok()?;
    (parsed.offset().local_minus_utc() == 0).then(|| parsed.with_timezone(&Utc))
}

fn task_id(value: &Value) -> Option<&str> {
    let id = value.as_str()?;
    let digits = id.strip_prefix('T')?;
    (!digits.is_empty() && digits.len() <= 20 && digits.bytes().all(|b| b.is_ascii_digit()))
        .then_some(id)
}

fn active(state: &Value) -> bool {
    state["status"] == "active" && state["assignee"].as_str().is_some_and(|s| !s.is_empty())
}

/// Project only the supplied bounded ledger page and task snapshot. `has_more`
/// describes reader pagination, not atomic completeness or historical coverage.
pub fn project(
    group: &str,
    events: &[Value],
    snapshots: &[Value],
    has_more: bool,
    transition_gap: bool,
    prices: Option<&PriceTable>,
) -> Value {
    let mut tasks: BTreeMap<String, Task> = BTreeMap::new();
    let mut seen: BTreeMap<String, Value> = BTreeMap::new();
    let mut duplicate_conflict = false;
    let mut unattributed = 0_u64;
    let mut turns = Vec::new();
    let snapshot_hash = serde_json::to_vec(events)
        .ok()
        .map(|bytes| format!("{:x}", Sha256::digest(bytes)));
    for event in events {
        if event["group_id"].as_str() != Some(group) {
            continue;
        }
        let Some(id) = event["id"].as_str().filter(|s| !s.is_empty()) else {
            duplicate_conflict = true;
            continue;
        };
        if let Some(previous) = seen.get(id) {
            duplicate_conflict |= previous != event;
            continue;
        }
        seen.insert(id.to_owned(), event.clone());
        if event["kind"] == "turn.usage" {
            turns.push(event);
        }
        if event["kind"] != "context.sync" {
            continue;
        }
        let Some(changes) = event["data"]["changes"].as_array() else {
            continue;
        };
        for change in changes {
            let transition = &change["task_transition"];
            if transition["schema"] != 1 {
                continue;
            }
            let Some(id) = task_id(&transition["task_id"]) else {
                continue;
            };
            let state = tasks.entry(id.to_owned()).or_default();
            let before = &transition["before"];
            let after = &transition["after"];
            if state.state.as_ref().is_some_and(|prev| prev != before) {
                state.invalid = true;
            }
            if before["status"] == "done"
                && (after["status"] == "active" || after["status"] == "planned")
            {
                state.reopened += 1;
            }
            if before["blocked"] == false && after["blocked"] == true {
                state.blockers += 1;
            }
            let Some(ts) = time(&event["ts"]) else {
                state.invalid = true;
                continue;
            };
            if !active(before) && active(after) {
                state.active = Some(ts);
            }
            if after["status"] == "done" && before["status"] != "done" {
                if let Some(start) = state.active.take() {
                    if start < ts {
                        state.intervals.push((start, ts));
                    } else {
                        state.invalid = true;
                    }
                }
            } else if !active(after) && after["status"] != "done" {
                state.active = None;
            }
            state.state = Some(after.clone());
        }
    }
    let mut cohorts: BTreeMap<(Option<String>, Option<String>), Cohort> = BTreeMap::new();
    let mut task_turns: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for event in turns {
        let data = &event["data"];
        let Some(id) = task_id(&data["task_ref"]) else {
            unattributed += 1;
            continue;
        };
        let model = data["model_id"].as_str().filter(|s| !s.is_empty());
        let runtime = data["runtime"]
            .as_str()
            .filter(|s| matches!(*s, "pi" | "claude"));
        let start = time(&data["started_at"]);
        let end = time(&data["ended_at"]);
        let membership = match (tasks.get(id), start, end) {
            (Some(task), Some(start), Some(end))
                if !task.invalid
                    && !duplicate_conflict
                    && !transition_gap
                    && start <= end
                    && task.intervals.iter().any(|(a, b)| *a < start && end < *b) =>
            {
                "observed_active_to_done"
            }
            _ => "unknown",
        };
        let tokens = [
            "input_tokens",
            "output_tokens",
            "cache_read_tokens",
            "cache_write_tokens",
        ]
        .map(|key| data[key].as_u64());
        let amount =
            prices.and_then(|table| table.numerator(model?, data["started_at"].as_str()?, tokens));
        task_turns.entry(id.to_owned()).or_default().push(json!({
            "event_id":event["id"], "model":model, "model_label":null, "runtime":runtime,
            "membership":membership, "status":data["status"], "tokens":tokens, "priced_numerator":amount.map(|v|v.to_string())
        }));
        let cohort = cohorts
            .entry((model.map(str::to_owned), runtime.map(str::to_owned)))
            .or_default();
        cohort.tasks.insert(id.to_owned());
        if membership != "observed_active_to_done" {
            cohort.unknown_membership += 1;
            continue;
        }
        cohort.turns += 1;
        for (sum, value) in cohort.tokens.iter_mut().zip(tokens) {
            if let Some(value) = value {
                *sum = sum.saturating_add(u128::from(value));
            }
        }
        if let Some(amount) = amount.and_then(|a| cohort.priced.checked_add(a)) {
            cohort.priced = amount;
            cohort.priced_turns += 1;
        } else {
            cohort.unpriced_turns += 1;
        }
    }
    let states: BTreeMap<&str, &Value> = snapshots
        .iter()
        .filter_map(|s| Some((task_id(&s["id"])?, s)))
        .collect();
    let mut ids: BTreeSet<String> = tasks.keys().cloned().collect();
    ids.extend(states.keys().map(|s| (*s).to_owned()));
    ids.extend(task_turns.keys().cloned());
    let task_results: Vec<Value> = ids.iter().map(|id| {
        let task = tasks.get(id);
        let state = states.get(id.as_str());
        let turns = task_turns.get(id).cloned().unwrap_or_default();
        let mut priced_sum = Some(0_u128);
        let mut priced_count = 0_u64;
        for turn in &turns {
            if turn["membership"] != "observed_active_to_done" { continue; }
            if let Some(amount) = turn["priced_numerator"].as_str().and_then(|v| v.parse::<u128>().ok()) {
                priced_sum = priced_sum.and_then(|sum| sum.checked_add(amount));
                priced_count += 1;
            }
        }
        let subtotal = if priced_count > 0 { priced_sum } else { None };
        json!({"task_id":id, "current_status":state.and_then(|s|s["status"].as_str()), "waiting_on":state.and_then(|s|s["waiting_on"].as_str()), "currently_declared_blocked":state.and_then(|s|s["blocked"].as_bool()),
            "claim_to_done":null, "history":"partial", "observed_intervals":task.map_or(0, |t|t.intervals.len()),
            "observed_reopens":task.map_or(0, |t|t.reopened), "observed_declared_blocker_episodes":task.map_or(0, |t|t.blockers),
            "turns":turns, "priced_turns":priced_count,
            "priced_subtotal_numerator":subtotal.map(|v|v.to_string()), "denominator":"1000000000000000",
            "priced_subtotal_minor_units":subtotal.and_then(|n|prices.and_then(|p|p.minor_units(n))).map(|n|n.to_string()),
            "turn_total":null, "cost_total":null})
    }).collect();
    let cohort_results: Vec<Value> = cohorts.iter().map(|((model, runtime), c)| {
        let done = c.tasks.iter().filter(|id| states.get(id.as_str()).is_some_and(|s|s["status"] == "done")).count();
        let reopened = c.tasks.iter().filter(|id| tasks.get(*id).is_some_and(|t|t.reopened > 0)).count();
        let blockers: u64 = c.tasks.iter().filter_map(|id| tasks.get(id)).map(|t|t.blockers).sum();
        json!({"model":model, "runtime":runtime, "model_label":null, "observed_task_participations":c.tasks.len(),
            "observed_currently_done":done, "observed_ever_reopened":reopened, "observed_declared_blocker_episodes":blockers,
            "observed_settled_turns":c.turns, "observed_token_subtotals":c.tokens.map(|n|n.to_string()),
            "priced_turns":c.priced_turns,"unpriced_turns":c.unpriced_turns, "unknown_membership_turns":c.unknown_membership,
            "priced_subtotal_numerator":(c.priced_turns > 0).then(|| c.priced.to_string()), "denominator":"1000000000000000",
            "priced_subtotal_minor_units":if c.priced_turns > 0 { prices.and_then(|p| p.minor_units(c.priced)).map(|n| n.to_string()) } else { None },
            "total_cost":null,"mean_per_done_task":null})
    }).collect();
    json!({"schema":1, "group_id":group, "ledger_snapshot_sha256":snapshot_hash,
        "ledger_events":events.len(), "has_more":has_more, "coverage":"partial",
        "snapshot_kind":"privacy-filtered event page", "first_event_id":events.first().and_then(|e|e["id"].as_str()),
        "last_event_id":events.last().and_then(|e|e["id"].as_str()), "window_start":events.first().and_then(|e|e["ts"].as_str()),
        "window_end":events.last().and_then(|e|e["ts"].as_str()),
        "coverage_reason":"context changes and ledger appends are not atomic; unsupported and unattributed turns may be missing",
        "duplicate_conflict":duplicate_conflict,"transition_gap":transition_gap,"unattributed_turns":unattributed,
        "tasks":task_results,"cohorts":cohort_results})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn captured() -> Vec<Value> {
        serde_json::from_str(include_str!("usage_report_captured.json")).unwrap()
    }

    #[test]
    fn captured_unattributed_turns_remain_unknown_without_a_hook_binding() {
        let events = captured();
        let output = project("g_example", &events, &[], true, false, None);
        assert_eq!(output["unattributed_turns"], 5);
        assert_eq!(output["coverage"], "partial");
        assert!(output["cohorts"].as_array().unwrap().is_empty());
    }

    #[test]
    fn captured_stock_events_without_hook_leave_usage_unknown() {
        let events = captured();
        let tasks: Vec<Value> =
            serde_json::from_str(include_str!("context/captured_tasks.json")).unwrap();
        let report = project("g_example", &events[5..], &tasks, true, true, None);
        assert_eq!(report["tasks"].as_array().unwrap().len(), 3);
        assert!(report["tasks"][0]["turn_total"].is_null());
        assert!(report["tasks"][0]["cost_total"].is_null());
        assert!(report["cohorts"].as_array().unwrap().is_empty());
    }

    #[test]
    fn labeled_capture_mutations_join_explicit_refs_only_and_deduplicate() {
        let events = captured();
        let mut opened = events[5].clone();
        let mut done = events[6].clone();
        let mut linked = events[0].clone();
        // Exercise future transition metadata by mutating a captured event
        // envelope; the capture has no linked usage or transition fields.
        opened["ts"] = json!("2026-11-02T18:27:08Z");
        done["ts"] = json!("2026-11-02T18:27:36Z");
        let planned = json!({"revision":0,"status":"planned","assignee":"actor-a","waiting_on":"actor","blocked":false});
        let active = json!({"revision":1,"status":"active","assignee":"actor-a","waiting_on":"actor","blocked":false});
        let finished = json!({"revision":2,"status":"done","assignee":"actor-a","waiting_on":"actor","blocked":false});
        opened["data"]["changes"][0]["task_transition"] =
            json!({"schema":1,"task_id":"T001","before":planned,"after":active});
        done["data"]["changes"][0]["task_transition"] =
            json!({"schema":1,"task_id":"T001","before":active,"after":finished});
        linked["data"]["task_ref"] = json!("T001");
        let output = project(
            "g_example",
            &[opened.clone(), linked.clone(), done.clone(), linked.clone()],
            &[],
            false,
            false,
            None,
        );
        assert_eq!(output["tasks"][0]["turns"].as_array().unwrap().len(), 1);
        assert_eq!(
            output["tasks"][0]["turns"][0]["membership"],
            "observed_active_to_done"
        );
        assert_eq!(output["cohorts"][0]["observed_settled_turns"], 1);
        assert!(output["cohorts"][0]["priced_subtotal_numerator"].is_null());
        assert!(output["tasks"][0]["cost_total"].is_null());
        // Illustrative user rates applied to the same captured counters.
        let price_input = json!({"version":"example-1","currency":"USD","minor_scale":2,"entries":[{
            "model":"claude-opus-5-5","effective_from":"2026-01-01T00:00:00Z",
            "effective_until":"2027-01-01T00:00:00Z","input":"1.5","output":"2",
            "cache_read":"0.125","cache_write":"0"}]});
        let prices = PriceTable::parse(&serde_json::to_vec(&price_input).unwrap()).unwrap();
        let priced = project(
            "g_example",
            &[opened.clone(), linked.clone(), done.clone()],
            &[],
            false,
            false,
            Some(&prices),
        );
        assert_eq!(
            priced["tasks"][0]["priced_subtotal_numerator"],
            "171674875000000"
        );
        assert_eq!(priced["tasks"][0]["priced_subtotal_minor_units"], "17");
        assert_eq!(
            priced["cohorts"][0]["priced_subtotal_numerator"],
            "171674875000000"
        );
        assert!(priced["cohorts"][0]["total_cost"].is_null());
        let gap = project(
            "g_example",
            &[opened.clone(), linked.clone(), done.clone()],
            &[],
            false,
            true,
            None,
        );
        assert_eq!(gap["tasks"][0]["turns"][0]["membership"], "unknown");
        let mut boundary = linked.clone();
        boundary["data"]["ended_at"] = done["ts"].clone();
        boundary["data"]["model_label"] = json!("changed-later");
        let boundary_result = project(
            "g_example",
            &[opened.clone(), boundary, done.clone()],
            &[],
            false,
            false,
            None,
        );
        assert_eq!(
            boundary_result["tasks"][0]["turns"][0]["membership"],
            "unknown"
        );
        assert!(boundary_result["tasks"][0]["turns"][0]["model_label"].is_null());
        let mut crossing = linked.clone();
        crossing["data"]["started_at"] = json!("2026-11-02T18:27:07Z");
        let crossed = project(
            "g_example",
            &[opened.clone(), crossing, done.clone()],
            &[],
            false,
            false,
            None,
        );
        assert_eq!(crossed["tasks"][0]["turns"][0]["membership"], "unknown");
        // The same task reference from another group is never joined.
        linked["group_id"] = json!("g_other");
        let output = project(
            "g_example",
            &[opened, linked, done],
            &[],
            false,
            false,
            None,
        );
        assert!(output["tasks"][0]["turns"].as_array().unwrap().is_empty());
    }
    #[test]
    fn a_conflicting_replay_never_proves_membership() {
        let capture = captured();
        let mut opened = capture[5].clone();
        let mut done = capture[6].clone();
        let mut linked = capture[0].clone();
        opened["ts"] = json!("2026-11-02T18:27:08Z");
        done["ts"] = json!("2026-11-02T18:27:36Z");
        let before = json!({"revision":0,"status":"planned","assignee":"actor-a","waiting_on":"actor","blocked":false});
        let active = json!({"revision":1,"status":"active","assignee":"actor-a","waiting_on":"actor","blocked":false});
        let after = json!({"revision":2,"status":"done","assignee":"actor-a","waiting_on":"actor","blocked":false});
        opened["data"]["changes"][0]["task_transition"] =
            json!({"schema":1,"task_id":"T001","before":before,"after":active});
        done["data"]["changes"][0]["task_transition"] =
            json!({"schema":1,"task_id":"T001","before":active,"after":after});
        linked["data"]["task_ref"] = json!("T001");
        let mut conflict = linked.clone();
        conflict["data"]["input_tokens"] = json!(7);
        let output = project(
            "g_example",
            &[opened, linked, done, conflict],
            &[],
            false,
            false,
            None,
        );
        assert_eq!(output["duplicate_conflict"], true);
        assert_eq!(output["tasks"][0]["turns"][0]["membership"], "unknown");
        assert_eq!(output["cohorts"][0]["unknown_membership_turns"], 1);
        assert!(output["cohorts"][0]["total_cost"].is_null());
    }
    #[test]
    fn labeled_multimodel_capture_keeps_overlapping_participations_separate() {
        let source = captured();
        let mut opened = source[5].clone();
        let mut done = source[6].clone();
        let mut first = source[0].clone();
        let mut second = source[1].clone();
        opened["ts"] = json!("2026-11-02T18:27:08Z");
        done["ts"] = json!("2026-11-02T18:27:36Z");
        let planned = json!({"revision":0,"status":"planned","assignee":"actor-a","waiting_on":"actor","blocked":false});
        let active = json!({"revision":1,"status":"active","assignee":"actor-a","waiting_on":"actor","blocked":false});
        let finished = json!({"revision":2,"status":"done","assignee":"actor-a","waiting_on":"actor","blocked":false});
        opened["data"]["changes"][0]["task_transition"] =
            json!({"schema":1,"task_id":"T001","before":planned,"after":active});
        done["data"]["changes"][0]["task_transition"] =
            json!({"schema":1,"task_id":"T001","before":active,"after":finished});
        first["data"]["task_ref"] = json!("T001");
        first["data"]["runtime"] = json!("claude");
        second["data"]["task_ref"] = json!("T001");
        second["data"]["model_id"] = json!("other-model");
        second["data"]["runtime"] = json!("pi");
        second["data"]["started_at"] = first["data"]["started_at"].clone();
        second["data"]["ended_at"] = first["data"]["ended_at"].clone();
        let snapshots: Vec<Value> =
            serde_json::from_str(include_str!("context/captured_tasks.json")).unwrap();
        let report = project(
            "g_example",
            &[opened, first, second, done],
            &snapshots,
            false,
            false,
            None,
        );
        let groups = report["cohorts"].as_array().unwrap();
        assert_eq!(groups.len(), 2);
        for group in groups {
            assert_eq!(group["observed_task_participations"], 1);
            assert_eq!(group["observed_currently_done"], 1);
            assert_eq!(group["observed_settled_turns"], 1);
            assert!(group["total_cost"].is_null());
            assert!(group["mean_per_done_task"].is_null());
        }
    }
    #[test]
    fn labeled_captured_transitions_distinguish_reopen_from_blocker_episode() {
        let source = captured();
        let mut events = source[5..10].to_vec();
        let states = [
            json!({"revision":0,"status":"planned","assignee":"actor-a","waiting_on":"actor","blocked":false}),
            json!({"revision":1,"status":"active","assignee":"actor-a","waiting_on":"actor","blocked":false}),
            json!({"revision":2,"status":"done","assignee":"actor-a","waiting_on":"actor","blocked":false}),
            json!({"revision":3,"status":"planned","assignee":"actor-a","waiting_on":"actor","blocked":false}),
            json!({"revision":4,"status":"active","assignee":"actor-a","waiting_on":"actor","blocked":true}),
            json!({"revision":5,"status":"done","assignee":"actor-a","waiting_on":"user","blocked":true}),
        ];
        for (index, event) in events.iter_mut().enumerate() {
            event["data"]["changes"][0]["task_transition"] = json!({"schema":1,"task_id":"T001","before":states[index],"after":states[index + 1]});
        }
        let report = project("g_example", &events, &[], false, false, None);
        assert_eq!(report["tasks"][0]["observed_intervals"], 2);
        assert_eq!(report["tasks"][0]["observed_reopens"], 1);
        assert_eq!(report["tasks"][0]["observed_declared_blocker_episodes"], 1);
        assert!(report["tasks"][0]["claim_to_done"].is_null());
    }
}
