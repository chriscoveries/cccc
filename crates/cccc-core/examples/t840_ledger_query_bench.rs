//! Query-latency probe for the real T609 ledger capture. Run only on claw scratch.
use cccc_core::ledger;
use serde::Deserialize;
use serde_json::json;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::Path;
use std::time::Instant;

#[derive(Deserialize)]
struct EventIdentity {
    id: String,
}

fn sample_ids(path: &Path) -> io::Result<Vec<String>> {
    let mut count = 0_usize;
    let mut line = Vec::new();
    let mut reader = BufReader::new(File::open(path)?);
    while reader.read_until(b'\n', &mut line)? != 0 {
        count += 1;
        line.clear();
    }
    if count == 0 {
        return Err(io::Error::other("ledger capture is empty"));
    }

    let mut positions = [0, count / 4, count / 2, count * 3 / 4, count - 1];
    positions.sort_unstable();
    let mut selected = Vec::new();
    let mut target = 0;
    let reader = BufReader::new(File::open(path)?);
    for (position, line) in reader.lines().enumerate() {
        if target == positions.len() {
            break;
        }
        if position == positions[target] {
            selected.push(serde_json::from_str::<EventIdentity>(&line?)?.id);
            target += 1;
        }
    }
    selected.dedup();
    Ok(selected)
}

fn p95_ms(mut samples: Vec<u128>) -> f64 {
    samples.sort_unstable();
    let rank = (samples.len() * 95).div_ceil(100).max(1);
    samples[rank - 1] as f64 / 1_000_000.0
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .ok_or("usage: t840_ledger_query_bench <real-ledger.jsonl>")?;
    let path = Path::new(&path);
    let ids = sample_ids(path)?;

    let build = Instant::now();
    let first = ledger::find_event(path, &ids[0])?.ok_or("sampled event missing")?;
    drop(first);
    let cold_index_ms = build.elapsed().as_secs_f64() * 1000.0;

    let mut point_samples = Vec::new();
    for _ in 0..20 {
        for id in &ids {
            let started = Instant::now();
            let event = ledger::find_event(path, id)?.ok_or("sampled event missing")?;
            drop(event);
            point_samples.push(started.elapsed().as_nanos());
        }
    }
    let point_p95_ms = p95_ms(point_samples.clone());

    let mut full_scan_samples = Vec::new();
    for _ in 0..3 {
        let started = Instant::now();
        let chat_messages = ledger::inspect(path, |events, _| {
            events
                .iter()
                .filter(|event| event.kind == "chat.message")
                .count()
        })?;
        if chat_messages == 0 {
            return Err("full scan returned no chat messages".into());
        }
        full_scan_samples.push(started.elapsed().as_nanos());
    }

    println!(
        "{}",
        json!({
            "source_bytes": std::fs::metadata(path)?.len(),
            "sampled_event_count": ids.len(),
            "cold_index_ms": cold_index_ms,
            "point_lookup_samples_ms": point_samples.iter().map(|n| *n as f64 / 1_000_000.0).collect::<Vec<_>>(),
            "point_lookup_p95_ms": point_p95_ms,
            "full_scan_samples_ms": full_scan_samples.iter().map(|n| *n as f64 / 1_000_000.0).collect::<Vec<_>>(),
            "full_scan_p95_ms": p95_ms(full_scan_samples),
            "retained_note": "cold index build excluded from warm-query samples"
        })
    );
    Ok(())
}
