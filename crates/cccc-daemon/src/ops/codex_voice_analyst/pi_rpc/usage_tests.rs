//! Usage regressions replay the upstream Pi captures. Variants only repeat
//! captured snapshots, omit optional fields, or combine captured observations.
use super::Value;
use super::events::{
    CAPTURED_LENGTH_STOP, CAPTURED_TOOL_TURN, replay_captured_records, replay_captured_turn,
};

fn records(captured: &str) -> Vec<Value> {
    captured
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("captured Pi record"))
        .filter(|record| record["type"] != "response")
        .collect()
}

fn completion(events: &[Value]) -> &Value {
    let completions = events
        .iter()
        .filter(|event| event["method"] == "turn/completed")
        .collect::<Vec<_>>();
    assert_eq!(completions.len(), 1, "one captured turn settles once");
    &completions[0]["params"]["turn"]
}

fn assert_tool_usage(usage: &Value) {
    // Final assistant snapshots in captured_tool_turn.jsonl report
    // 1642 + 210 input, 78 + 12 output, and 0 + 1536 cached input.
    assert_eq!(usage["source"], "pi_rpc");
    assert_eq!(usage["model_id"], "deepseek-v4.1-flash");
    assert_eq!(usage["input_tokens"], 1852);
    assert_eq!(usage["output_tokens"], 90);
    assert_eq!(usage["cache_read_tokens"], 1536);
    assert_eq!(usage["cache_write_tokens"], 0);
    assert_eq!(usage["usage_samples"], 2);
}

#[test]
fn captured_pi_usage_sums_assistant_messages_across_tool_execution() {
    let events = replay_captured_turn(CAPTURED_TOOL_TURN, "captured");
    let usage = &completion(&events)["usage"];
    assert_tool_usage(usage);
    let started = usage["started_at"].as_str().expect("start time");
    let ended = usage["ended_at"].as_str().expect("settlement time");
    assert!(!started.is_empty() && ended >= started);
    assert_eq!(usage.as_object().expect("selected usage fields").len(), 9);
    assert!(
        events
            .iter()
            .filter(|event| event["method"] != "turn/completed")
            .all(|event| event.pointer("/params/turn/usage").is_none())
    );
}

#[test]
fn captured_pi_repeated_message_and_turn_snapshots_do_not_add_usage() {
    let mut repeated = Vec::new();
    for record in records(CAPTURED_TOOL_TURN) {
        repeated.push(record.clone());
        if record["type"] == "message_end" && record["message"]["role"] == "assistant" {
            repeated.push(record.clone());
        }
        if record["type"] == "turn_end" {
            repeated.push(record);
        }
    }
    let events = replay_captured_records(&repeated, "captured", false);
    assert_tool_usage(&completion(&events)["usage"]);
}

#[test]
fn captured_pi_message_timestamps_deduplicate_without_response_ids() {
    // Older providers may omit responseId. Keep each captured timestamp and
    // replay the same final message snapshot twice, without inventing an ID.
    let mut repeated = Vec::new();
    for mut record in records(CAPTURED_TOOL_TURN) {
        if let Some(message) = record["message"].as_object_mut() {
            message.remove("responseId");
        }
        repeated.push(record.clone());
        if record["type"] == "message_end" && record["message"]["role"] == "assistant" {
            repeated.push(record);
        }
    }
    let events = replay_captured_records(&repeated, "captured", false);
    assert_tool_usage(&completion(&events)["usage"]);
}

#[test]
fn captured_pi_omitted_counter_is_unknown_while_reported_zero_stays_zero() {
    let mut captured = records(CAPTURED_TOOL_TURN);
    let final_message = captured
        .iter_mut()
        .rev()
        .find(|record| record["type"] == "message_end" && record["message"]["role"] == "assistant")
        .expect("captured answer");
    final_message["message"]["usage"]
        .as_object_mut()
        .expect("captured usage")
        .remove("input");
    let events = replay_captured_records(&captured, "captured", false);
    let usage = &completion(&events)["usage"];
    assert!(usage["input_tokens"].is_null());
    assert_eq!(usage["output_tokens"], 90);
    assert_eq!(usage["cache_read_tokens"], 1536);
    assert_eq!(usage["cache_write_tokens"], 0);
    assert_eq!(usage["usage_samples"], 2);
}

#[test]
fn captured_pi_message_without_usage_keeps_all_totals_unknown() {
    let mut captured = records(CAPTURED_TOOL_TURN);
    captured
        .iter_mut()
        .find(|record| record["type"] == "message_end" && record["message"]["role"] == "assistant")
        .expect("captured tool request")["message"]
        .as_object_mut()
        .expect("captured assistant message")
        .remove("usage");
    let events = replay_captured_records(&captured, "captured", false);
    let usage = &completion(&events)["usage"];
    for field in [
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
    ] {
        assert!(usage[field].is_null(), "{field} cannot be a partial sum");
    }
    assert_eq!(usage["usage_samples"], 2);
}

#[test]
fn captured_pi_mixed_or_missing_models_remain_unknown() {
    // Replay a final assistant snapshot from the length-stop capture inside
    // the tool capture's owned turn: both model names and usages are captured.
    let additional = records(CAPTURED_LENGTH_STOP)
        .into_iter()
        .find(|record| record["type"] == "message_end" && record["message"]["role"] == "assistant")
        .expect("captured other model");
    let mut mixed = records(CAPTURED_TOOL_TURN);
    let settlement = mixed.pop().expect("captured settlement");
    mixed.push(additional);
    mixed.push(settlement);
    let events = replay_captured_records(&mixed, "captured", false);
    let usage = &completion(&events)["usage"];
    assert!(usage["model_id"].is_null());
    assert_eq!(usage["input_tokens"], 2281);
    assert_eq!(usage["output_tokens"], 91);
    assert_eq!(usage["cache_read_tokens"], 2816);
    assert_eq!(usage["usage_samples"], 3);

    for remove_all in [false, true] {
        let mut unknown = records(CAPTURED_TOOL_TURN);
        let mut removed = false;
        for record in &mut unknown {
            if record["type"] == "message_end"
                && record["message"]["role"] == "assistant"
                && (remove_all || !removed)
            {
                record["message"]
                    .as_object_mut()
                    .expect("assistant")
                    .remove("model");
                removed = true;
            }
        }
        let events = replay_captured_records(&unknown, "captured", false);
        assert!(completion(&events)["usage"]["model_id"].is_null());
        assert_eq!(completion(&events)["usage"]["input_tokens"], 1852);
    }
}

#[test]
fn captured_pi_failure_and_owned_cancellation_retain_reported_usage() {
    let failed = replay_captured_turn(CAPTURED_LENGTH_STOP, "captured");
    let turn = completion(&failed);
    assert_eq!(turn["status"], "failed");
    assert_eq!(turn["usage"]["input_tokens"], 429);
    assert_eq!(turn["usage"]["output_tokens"], 1);
    assert_eq!(turn["usage"]["cache_read_tokens"], 1280);
    assert_eq!(turn["usage"]["usage_samples"], 1);
    let cancelled = replay_captured_records(&records(CAPTURED_TOOL_TURN), "captured", true);
    let turn = completion(&cancelled);
    assert_eq!(turn["status"], "interrupted");
    assert_tool_usage(&turn["usage"]);
}
