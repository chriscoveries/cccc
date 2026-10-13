//! Isolated T539 measurement through the real daemon's IPC polling handler.
//! Creates a new home, starts no actor processes, and writes only its copy.
//! Usage: ledger_poll_soak NEW_CASE LEDGER_COPY_OR_DASH ROUNDS [ACTOR_ID]
//!                        [START_PAUSE_MS] [POLL_PAUSE_MS] [GROUPS]
//! A dash generates approximately 91 MiB of accepted delivery history.

use cccc_client::DaemonClient;
use cccc_contracts::{Actor, ActorRuntime, DaemonRequest, Event, GroupState};
use cccc_core::{GroupStore, HomeLayout};
use fs2::FileExt;
use serde_json::json;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::time::{Duration, Instant};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() < 3 {
        return Err(
            "usage: ledger_poll_soak NEW_CASE LEDGER_COPY_OR_DASH ROUNDS [ACTOR_ID] [START_PAUSE_MS] [POLL_PAUSE_MS] [GROUPS]".into(),
        );
    }
    let case = Path::new(&args[0]);
    // create_dir fails if the caller points at any existing home.
    std::fs::create_dir(case)?;
    let home = HomeLayout::from_path(case.join("home"))?;
    let store = GroupStore::new(home.clone())?;
    let mut group = store.create("T539 isolated ledger polling", "")?;
    let actor_id = args.get(3).map(String::as_str).unwrap_or("measure-peer");
    let mut actor = Actor::new(actor_id);
    actor.runtime = ActorRuntime::WebModel;
    actor.normalize_runtime_constraints();
    group.actors.push(actor);
    group.running = true;
    group.state = GroupState::Active;
    store.save(&group)?;
    let path = store.ledger_path(&group.group_id)?;
    if args[1] == "-" {
        generate(&path, &group.group_id, actor_id)?;
    } else {
        // Read the supplied copy; never modify it or copy its runnable group
        // configuration, account state, connections, or automation rules.
        std::fs::copy(&args[1], &path)?;
    }
    let rounds: usize = args[2].parse()?;
    let start_pause = args
        .get(4)
        .map(|value| value.parse::<u64>())
        .transpose()?
        .unwrap_or(0);
    let poll_pause = args
        .get(5)
        .map(|value| value.parse::<u64>())
        .transpose()?
        .unwrap_or(0);
    let group_count = args
        .get(6)
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(1);
    if !(1..=4).contains(&group_count) {
        return Err("GROUPS must be between 1 and 4".into());
    }
    let request = DaemonRequest {
        v: 1,
        op: "runtime_wait_next_turn".into(),
        args: json!({"group_id":group.group_id,"actor_id":actor_id,"by":actor_id,"limit":1})
            .as_object()
            .cloned()
            .ok_or("request object")?,
    };
    let mut cases = vec![(group, path.clone(), request)];
    for index in 1..group_count {
        // Alternating large groups also exercises the shared index budget;
        // a single protected oversized index would not expose that churn.
        let mut other = store.create(&format!("T539 isolated ledger polling {index}"), "")?;
        other.actors = cases[0].0.actors.clone();
        other.running = true;
        other.state = GroupState::Active;
        store.save(&other)?;
        let other_path = store.ledger_path(&other.group_id)?;
        std::fs::copy(&path, &other_path)?;
        let mut request = cases[0].2.clone();
        request
            .args
            .insert("group_id".into(), json!(other.group_id));
        cases.push((other, other_path, request));
    }
    println!("phase,pid,elapsed_ms,cpu_ticks,rss_kib,hwm_kib,swap_kib,threads,ledger_bytes");
    let started = Instant::now();
    sample("cold", started, &path)?;
    // Each roster contains only one unbound Web Model actor. It is structured
    // and has no provider command; daemon restore does not launch or push it.
    let daemon_home = home.clone();
    let daemon = tokio::spawn(async move { cccc_daemon::run(daemon_home).await });
    let client = DaemonClient::new(home.clone());
    let ping = DaemonRequest {
        v: 1,
        op: "ping".into(),
        args: Default::default(),
    };
    let ready_deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if daemon.is_finished() {
            return Err(format!("daemon stopped before readiness: {:?}", daemon.await).into());
        }
        if client
            .clone()
            .with_timeout(Duration::from_secs(2))
            .call(&ping)
            .await
            .is_ok_and(|response| response.ok)
        {
            break;
        }
        if Instant::now() >= ready_deadline {
            return Err("isolated daemon did not become ready within 60 seconds".into());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    sample("ready", started, &path)?;
    eprintln!(
        "ready pid={} home={} actor={actor_id} groups={group_count}",
        std::process::id(),
        home.root().display()
    );
    tokio::time::sleep(Duration::from_millis(start_pause)).await;
    for round in 0..=rounds {
        for (index, (group, path, request)) in cases.iter().enumerate() {
            if round > 0 {
                let lock = OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .open(store.state_dir(&group.group_id)?.join("ledger/ledger.lock"))?;
                lock.lock_exclusive()?;
                let mut file = OpenOptions::new().append(true).open(&path)?;
                // Two externally committed receipts model missed/out-of-order
                // notifications. No in-process append hook updates the index.
                for offset in 0..2 {
                    let event = receipt(
                        &group.group_id,
                        actor_id,
                        &format!("delta-{round}-{offset}"),
                    );
                    serde_json::to_writer(&mut file, &event)?;
                    file.write_all(b"\n")?;
                }
                file.sync_data()?;
                drop(lock);
            }
            let response = client.call(request).await?;
            if !response.ok {
                return Err(format!("runtime polling failed: {:?}", response.error).into());
            }
            // On a copied ledger an actual pending turn can be claimed. That is
            // useful functional coverage, but subsequent in-progress reads would
            // bypass the polling scan and invalidate the performance comparison.
            if response
                .result
                .get("turn")
                .is_some_and(|turn| !turn.is_null())
                || response
                    .result
                    .get("status")
                    .and_then(|status| status.as_str())
                    == Some("turn_in_progress")
            {
                return Err("fixture has pending work; select an actor with settled history for the polling comparison".into());
            }
            sample(
                &format!("{}_{index}", if round == 0 { "warm" } else { "poll" }),
                started,
                path,
            )?;
            tokio::time::sleep(Duration::from_millis(poll_pause)).await;
        }
    }
    let shutdown = DaemonRequest {
        v: 1,
        op: "shutdown".into(),
        args: json!({"expected_pid": std::process::id()})
            .as_object()
            .cloned()
            .ok_or("shutdown object")?,
    };
    if !client.call(&shutdown).await?.ok {
        return Err("isolated daemon refused its PID-bound shutdown".into());
    }
    daemon.await??;
    Ok(())
}

fn receipt(group: &str, actor: &str, source: &str) -> Event {
    let mut event = Event::new("runtime.delivery", group);
    event.by = "system".into();
    event.data =
        json!({"actor_id":actor,"source_event_id":source,"state":"accepted","transport":"pty"})
            .as_object()
            .cloned()
            .expect("receipt object");
    event
}

fn generate(path: &Path, group: &str, actor: &str) -> io::Result<()> {
    let mut file = BufWriter::new(File::create(path)?);
    let body = "x".repeat(2048);
    let mut written = 0;
    let mut index = 0;
    while written < 91 * 1024 * 1024 {
        let mut source = Event::new("chat.message", group);
        source.by = "user".into();
        source.data = json!({"to":[actor],"message_mode":"send","text":body})
            .as_object()
            .cloned()
            .expect("message object");
        let accepted = receipt(group, actor, &source.id);
        for event in [&source, &accepted] {
            let bytes = serde_json::to_vec(event).map_err(io::Error::other)?;
            written += bytes.len() + 1;
            file.write_all(&bytes)?;
            file.write_all(b"\n")?;
        }
        index += 1;
    }
    file.flush()?;
    file.get_ref().sync_data()?;
    eprintln!("synthetic fixture: {written} bytes, {index} settled messages");
    Ok(())
}

fn sample(phase: &str, started: Instant, path: &Path) -> io::Result<()> {
    let stat = std::fs::read_to_string("/proc/self/stat")?;
    let (_, fields) = stat
        .rsplit_once(')')
        .ok_or_else(|| io::Error::other("process stat"))?;
    let fields = fields.split_whitespace().collect::<Vec<_>>();
    let cpu = fields[11].parse::<u64>().map_err(io::Error::other)?
        + fields[12].parse::<u64>().map_err(io::Error::other)?;
    let status = std::fs::read_to_string("/proc/self/status")?;
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or_default()
    };
    println!(
        "{phase},{},{},{cpu},{},{},{},{},{}",
        std::process::id(),
        started.elapsed().as_millis(),
        field("VmRSS:"),
        field("VmHWM:"),
        field("VmSwap:"),
        field("Threads:"),
        std::fs::metadata(path)?.len()
    );
    io::stdout().flush()
}
