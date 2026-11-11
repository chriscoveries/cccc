//! CI-only before/after replay of the retained, redacted ledger fixture.
use cccc_contracts::Event;
use cccc_core::ledger;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::time::Instant;

fn memory(field: &str) -> Option<u64> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| {
            line.strip_prefix(field)?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
}

struct HashWriter<'a>(&'a mut Sha256);
impl Write for HashWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn hash_event(hash: &mut Sha256, event: &Event) -> io::Result<()> {
    serde_json::to_writer(HashWriter(hash), event).map_err(io::Error::other)
}

fn main() -> io::Result<()> {
    let workload = std::env::args_os()
        .nth(1)
        .ok_or_else(|| io::Error::other("expected workload path"))?;
    let workload = Path::new(&workload);
    let records = BufReader::new(File::open(workload)?)
        .lines()
        .try_fold(0_usize, |count, line| line.map(|_| count + 1))?;
    if records < 100 {
        return Err(io::Error::other("expected retained workload"));
    }
    let initial = records * 3 / 4;
    let boundary_rows: Vec<_> = (0..=8)
        .map(|part| initial + (records - initial) * part / 8)
        .collect();
    let mut boundaries = Vec::new();
    let mut samples = Vec::new();
    let mut bytes = 0_u64;
    let mut reader = BufReader::new(File::open(workload)?);
    let mut line = String::new();
    for row in 1..=records {
        line.clear();
        bytes += reader.read_line(&mut line)? as u64;
        if boundary_rows.contains(&row) || (row <= initial && row % (initial / 32) == 0) {
            let event: Event = serde_json::from_str(&line).map_err(io::Error::other)?;
            if boundary_rows.contains(&row) {
                boundaries.push((bytes, event.id.clone()));
            }
            if row <= initial {
                samples.push(event.id);
            }
        }
    }
    drop(reader);
    drop(line);
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("ledger.jsonl");
    let mut input = File::open(workload)?;
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)?;
    io::copy(
        &mut Read::by_ref(&mut input).take(boundaries[0].0),
        &mut output,
    )?;
    output.sync_data()?;
    let mut fingerprint = Sha256::new();
    let begin = Instant::now();
    let event = ledger::find_event(&path, &boundaries[0].1)?
        .ok_or_else(|| io::Error::other("missing cold event"))?;
    let cold_ms = begin.elapsed().as_secs_f64() * 1000.0;
    hash_event(&mut fingerprint, &event)?;
    drop(event);
    let mut catchup_ms = Vec::new();
    let mut previous = boundaries[0].0;
    for (end, id) in &boundaries[1..] {
        // Deliberately no append notification: exercise committed-byte catch-up.
        io::copy(
            &mut Read::by_ref(&mut input).take(end - previous),
            &mut output,
        )?;
        output.sync_data()?;
        let begin = Instant::now();
        let event = ledger::find_event(&path, id)?
            .ok_or_else(|| io::Error::other("missing appended event"))?;
        catchup_ms.push(begin.elapsed().as_secs_f64() * 1000.0);
        hash_event(&mut fingerprint, &event)?;
        previous = *end;
    }
    let begin = Instant::now();
    for id in &samples {
        let event =
            ledger::find_event(&path, id)?.ok_or_else(|| io::Error::other("missing warm event"))?;
        hash_event(&mut fingerprint, &event)?;
        for event in ledger::events_after(&path, id, 3)? {
            hash_event(&mut fingerprint, &event)?;
        }
    }
    let warm_ms = begin.elapsed().as_secs_f64() * 1000.0;
    let steady_rss_kib = memory("VmRSS:");
    let query_peak_kib = memory("VmHWM:");
    let begin = Instant::now();
    let all = ledger::read_all(&path)?;
    if all.len() != records {
        return Err(io::Error::other("record census changed"));
    }
    for event in &all {
        hash_event(&mut fingerprint, event)?;
    }
    drop(all);
    ledger::inspect_status(&path, |events, positions, generations| {
        let mut positions: Vec<_> = positions.iter().collect();
        let mut generations: Vec<_> = generations.iter().collect();
        positions.sort_unstable();
        generations.sort_unstable();
        serde_json::to_writer(
            HashWriter(&mut fingerprint),
            &(events.len(), positions, generations),
        )
    })?
    .map_err(io::Error::other)?;
    let bulk_ms = begin.elapsed().as_secs_f64() * 1000.0;
    println!(
        "{}",
        json!({"records":records,"bytes":bytes,"cold_ms":cold_ms,"catchup_ms":catchup_ms,
        "warm_lookup_and_page_ms":warm_ms,"bulk_ms":bulk_ms,"steady_rss_kib":steady_rss_kib,
        "query_peak_kib":query_peak_kib,"peak_including_bulk_kib":memory("VmHWM:"),
        "rss_after_bulk_kib":memory("VmRSS:"),"correctness_sha256":format!("{:x}", fingerprint.finalize())})
    );
    Ok(())
}
