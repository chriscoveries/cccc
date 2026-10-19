//! Isolated PTY workload for RSS/heap profiling; no CCCC production home.
use cccc_contracts::RunnerKind;
use cccc_runtime::{HistoryConfig, LaunchSpec, TerminalAttachMode};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let duration: u64 = args.get(1).map_or(Ok(120), |s| s.parse())?;
    let actors: usize = args.get(2).map_or(Ok(18), |s| s.parse())?;
    let write_bytes: usize = args.get(3).map_or(Ok(2), |s| s.parse())?;
    let interval_ms: u64 = args.get(4).map_or(Ok(1), |s| s.parse())?;
    let cols: u16 = args.get(5).map_or(Ok(80), |s| s.parse())?;
    let rows: u16 = args.get(6).map_or(Ok(24), |s| s.parse())?;
    let root = PathBuf::from(args.get(7).map_or("/tmp/sol-b-pty-ram", String::as_str));
    let mode = args.get(8).map_or("text", String::as_str);
    std::fs::create_dir_all(&root)?;
    let emitter = "import os,sys,time\nsize,delay,seconds=map(float,sys.argv[1:4])\nmode=sys.argv[4]\nchunk=b'x'*int(size) if mode=='osc' else b'x'*(int(size)-1)+b'\\n'\nif mode=='osc': os.write(1,b'\\x1b]2;')\nend=time.monotonic()+seconds\nwhile time.monotonic()<end:\n os.write(1,chunk)\n time.sleep(delay/1000)\n";
    println!(
        "pid={},duration={duration},actors={actors},write_bytes={write_bytes},interval_ms={interval_ms},cols={cols},rows={rows}",
        std::process::id()
    );
    for actor in 0..actors {
        let mut env = BTreeMap::new();
        // Profile the host's runtime allocations, not the Python children.
        env.insert("LD_PRELOAD".into(), String::new());
        cccc_runtime::start_with_history(
            LaunchSpec {
                group_id: "sol-b-ram".into(),
                actor_id: format!("a{actor}"),
                runner: RunnerKind::Pty,
                command: vec![
                    "python3".into(),
                    "-u".into(),
                    "-c".into(),
                    emitter.into(),
                    write_bytes.to_string(),
                    interval_ms.to_string(),
                    (duration + 10).to_string(),
                    mode.into(),
                ],
                cwd: root.clone(),
                env,
                cols,
                rows,
            },
            HistoryConfig {
                path: root.join(format!("a{actor}/session.pty")),
                max_bytes: 10 * 1024 * 1024,
                hot_bytes: 2_000_000,
                persist: true,
            },
        )?;
    }
    let start = Instant::now();
    println!("seconds,rss_kib,total_cursor,retained_sample_bytes,snapshot_bytes");
    loop {
        let seconds = start.elapsed().as_secs();
        let status = std::fs::read_to_string("/proc/self/status")?;
        let rss = status
            .lines()
            .find(|s| s.starts_with("VmRSS:"))
            .unwrap_or_default();
        let mut cursor = 0_u64;
        let mut snapshots = 0;
        for actor in 0..actors {
            let id = format!("a{actor}");
            cursor += cccc_runtime::history("sol-b-ram", &id, None, 1)?.end_cursor;
            let terminal = cccc_runtime::attach_with_snapshot(
                "sol-b-ram",
                &id,
                TerminalAttachMode::Viewer,
                false,
                None,
            )?;
            snapshots += terminal.initial_output_bytes();
        }
        println!(
            "{seconds},{},{cursor},1,{snapshots}",
            rss.split_whitespace().nth(1).unwrap_or("0")
        );
        if seconds >= duration {
            break;
        }
        std::thread::sleep(Duration::from_secs(10).min(Duration::from_secs(duration - seconds)));
    }
    // Keep registry allocations live at process exit so heaptrack can attribute
    // them. Actor emitters have finite lifetimes and the owned-tree guard also
    // terminates them when this isolated host exits.
    Ok(())
}
