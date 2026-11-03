use std::io::Read;

use anyhow::{Context, Result, bail};
use cccc_client::DaemonClient;
use cccc_core::{HomeLayout, usage_price::PriceTable, usage_report};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    args::UsageArgs,
    commands::common::{call, group},
};

pub async fn run(client: &DaemonClient, home: &HomeLayout, args: UsageArgs) -> Result<()> {
    let group_id = group(home, args.group_id)?;
    if let Some(task) = &args.task {
        let digits = task.strip_prefix('T').unwrap_or_default();
        if digits.is_empty() || digits.len() > 20 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            bail!("task must be a canonical T-number");
        }
    }
    let (prices, price_identity) = if let Some(path) = args.prices {
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(256 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        let parsed = PriceTable::parse(&bytes).context("invalid price table")?;
        let identity = json!({"sha256":format!("{:x}",Sha256::digest(&bytes)),"version":parsed.version,
            "currency":parsed.currency,"minor_scale":parsed.minor_scale});
        (Some(parsed), identity)
    } else {
        (None, Value::Null)
    };
    let response = call(
        client,
        "usage_report_source",
        json!({"group_id":group_id,"task_id":args.task}),
    )
    .await?;
    if !response.ok {
        bail!(
            "usage source unavailable: {}",
            response.error.map_or_else(
                || "unknown error".into(),
                |e| format!("{}: {}", e.code, e.message)
            )
        );
    }
    let events = response
        .result
        .get("events")
        .and_then(Value::as_array)
        .context("missing usage events")?;
    let snapshot_available = response.result.get("tasks").is_some_and(Value::is_array);
    let mut tasks = response
        .result
        .get("tasks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(task) = &args.task {
        if !tasks
            .iter()
            .any(|entry| entry["id"].as_str() == Some(task.as_str()))
        {
            tasks.push(json!({"id":task,"status":null}));
        }
    }
    let mut report = usage_report::project(
        &group_id,
        events,
        &tasks,
        response
            .result
            .get("has_more")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        response
            .result
            .get("transition_gap")
            .and_then(Value::as_bool)
            .unwrap_or(true)
            || response
                .result
                .get("changes_truncated")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        prices.as_ref(),
    );
    report["price_table"] = price_identity;
    report["ledger_snapshot_available"] = response
        .result
        .get("ledger_snapshot_available")
        .cloned()
        .unwrap_or(json!(false));
    report["task_snapshot_available"] = json!(snapshot_available);
    report["task_snapshot_sha256"] = if snapshot_available {
        json!(format!("{:x}", Sha256::digest(serde_json::to_vec(&tasks)?)))
    } else {
        Value::Null
    };
    report["canonical_ledger_page_sha256"] = response
        .result
        .get("canonical_page_sha256")
        .cloned()
        .unwrap_or(Value::Null);
    report["canonical_ledger_page_hash_domain"] = json!("length-prefixed-event-json-v1");
    report["ledger_fetched"] = response
        .result
        .get("fetched")
        .cloned()
        .unwrap_or(Value::Null);
    report["changes_truncated"] = response
        .result
        .get("changes_truncated")
        .cloned()
        .unwrap_or(json!(true));
    if let Some(task) = args.task {
        if let Some(items) = report["tasks"].as_array_mut() {
            items.retain(|entry| entry["task_id"] == task);
        }
        report.as_object_mut().unwrap().remove("cohorts");
    } else {
        report.as_object_mut().unwrap().remove("tasks");
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        // Deterministic and complete: text uses the same fields as JSON.
        println!(
            "Observed usage (coverage: partial; totals unknown)\n{}",
            serde_json::to_string_pretty(&report)?
        );
    }
    Ok(())
}
