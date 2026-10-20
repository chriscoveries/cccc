//! Narrow allocation probe using recorded production ledger/terminal data only.
//! Run on claw through claw-scratch. Never start a daemon or a provider runtime.
//! Terminal replay preserves captured payload bytes, but not original PTY read
//! boundaries or historical arrival timing; it is not a production slope test.
use cccc_contracts::RunnerKind;
use cccc_runtime::{HistoryConfig, LaunchSpec};
use serde_json::json;
use std::collections::BTreeMap;
use std::error::Error;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn sample(phase: &str, extra: serde_json::Value) -> Result<(), Box<dyn Error>> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    let mut memory = BTreeMap::new();
    for line in status.lines() {
        if let Some((name, value)) = line.split_once(':')
            && matches!(name, "VmRSS" | "RssAnon" | "RssFile" | "VmSwap" | "Threads")
        {
            memory.insert(name, value.trim().to_owned());
        }
    }
    println!(
        "{}",
        json!({"phase": phase, "pid": std::process::id(), "memory": memory, "extra": extra})
    );
    Ok(())
}

fn ledger(path: &Path) -> Result<(), Box<dyn Error>> {
    let mut line = String::new();
    BufReader::new(std::fs::File::open(path)?).read_line(&mut line)?;
    let event: serde_json::Value = serde_json::from_str(&line)?;
    let id = event["id"]
        .as_str()
        .ok_or("recorded first event lacks id")?
        .to_owned();
    drop(event);
    drop(line);
    sample(
        "before-index",
        json!({"source_bytes": std::fs::metadata(path)?.len()}),
    )?;
    for iteration in 0..2 {
        let start = Instant::now();
        let event = cccc_core::ledger::find_event(path, &id)?;
        if event.as_ref().map(|event| event.id.as_str()) != Some(id.as_str()) {
            return Err("recorded event not returned by index".into());
        }
        drop(event);
        sample(
            "after-index-query",
            json!({"iteration": iteration, "wall_ms": start.elapsed().as_millis()}),
        )?;
    }
    std::thread::sleep(Duration::from_secs(5));
    sample(
        "index-idle-5s",
        json!({"limit": "Cache remains a live static root; exit allocations are not necessarily unreachable leaks."}),
    )?;
    Ok(())
}

fn pty(root: &Path) -> Result<(), Box<dyn Error>> {
    let inputs = [
        root.join("terminal-initial.pty"),
        root.join("terminal-delta.pty"),
    ];
    let expected: u64 = inputs
        .iter()
        .map(std::fs::metadata)
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .map(std::fs::Metadata::len)
        .sum();
    if expected <= 2_000_000 {
        return Err("real capture must exceed the measured hot-history cap".into());
    }
    let emitter = "import os,sys,time,tty\ntty.setraw(1)\nfor path in sys.argv[1:]:\n with open(path,'rb') as f:\n  while True:\n   data=f.read(65536)\n   if not data: break\n   while data:\n    n=os.write(1,data); data=data[n:]\n   time.sleep(0.02)\ntime.sleep(120)\n";
    let mut command = vec![
        "python3".to_owned(),
        "-u".to_owned(),
        "-c".to_owned(),
        emitter.to_owned(),
    ];
    command.extend(
        inputs
            .iter()
            .map(|path| path.to_string_lossy().into_owned()),
    );
    let group = "g_1a9e39785e1a";
    let actor = "lab-luna-b";
    sample(
        "before-pty",
        json!({"recorded_bytes": expected, "actors": 1, "hot_bytes": 2_000_000, "cols": 80, "rows": 24}),
    )?;
    cccc_runtime::start_with_history(
        LaunchSpec {
            group_id: group.into(),
            actor_id: actor.into(),
            runner: RunnerKind::Pty,
            command,
            cwd: std::env::current_dir()?,
            env: BTreeMap::new(),
            cols: 80,
            rows: 24,
        },
        HistoryConfig {
            path: std::env::current_dir()?.join("replay.pty"),
            max_bytes: 2_000_000,
            hot_bytes: 2_000_000,
            persist: false,
        },
    )?;
    let result = (|| -> Result<(), Box<dyn Error>> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let page = cccc_runtime::history(group, actor, None, 1)?;
            if page.end_cursor > expected {
                return Err("replay generated unexpected extra output bytes".into());
            }
            if page.end_cursor == expected {
                break;
            }
            if Instant::now() > deadline {
                return Err("recorded PTY replay timed out".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let page = cccc_runtime::history(group, actor, None, 2_000_000)?;
        let retained = page.end_cursor - page.start_cursor;
        if retained != 2_000_000 || page.end_cursor != expected {
            return Err("recorded replay did not fill the actual hot-history cap".into());
        }
        drop(page);
        sample(
            "pty-filled",
            json!({"recorded_bytes_consumed": expected, "retained_output_bytes": retained}),
        )?;
        std::thread::sleep(Duration::from_secs(5));
        sample(
            "pty-idle-5s",
            json!({"no_more_input": true, "limit": "One recorded stream; no long-run or production plateau claim."}),
        )?;
        Ok(())
    })();
    let stopped = cccc_runtime::stop(group, actor);
    result?;
    stopped?;
    sample(
        "after-pty-stop",
        json!({"limit": "Completed history and mirror can remain retained under the existing policy."}),
    )?;
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args_os().collect();
    let mode = args
        .get(1)
        .and_then(|arg| arg.to_str())
        .ok_or("mode required: ledger|pty")?;
    let path = PathBuf::from(args.get(2).ok_or("recorded input path required")?).canonicalize()?;
    match mode {
        "ledger" => ledger(&path),
        "pty" => pty(&path),
        _ => Err("mode must be ledger or pty".into()),
    }
}
