use cccc_contracts::utc_now;
use fs2::FileExt;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
use std::io::{Read, Seek, SeekFrom};
#[cfg(target_os = "linux")]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
#[cfg(target_os = "linux")]
use std::process::{Command, Stdio};
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

use crate::fs::{read_json, write_json};
use crate::ledger;
use crate::{GroupStore, HomeLayout};

#[derive(Debug, Clone, Serialize)]
pub struct LedgerSnapshot {
    pub v: u8,
    pub group_id: String,
    pub created_at: String,
    pub reason: String,
    pub event_count: usize,
    pub last_event_id: String,
    pub sha256: String,
    pub path: String,
}

pub fn snapshot(home: &HomeLayout, group_id: &str, reason: &str) -> io::Result<LedgerSnapshot> {
    let store = GroupStore::new(home.clone())?;
    let ledger_path = store.ledger_path(group_id)?;
    let lock = ledger::acquire_writer_lock(&ledger_path)?;
    let result = snapshot_locked(&store, group_id, reason, &ledger_path);
    let unlock = FileExt::unlock(&lock);
    result.and_then(|value| unlock.map(|()| value))
}

fn snapshot_locked(
    store: &GroupStore,
    group_id: &str,
    reason: &str,
    ledger_path: &Path,
) -> io::Result<LedgerSnapshot> {
    // Validate and hash the canonical Event array in one pass. Maintenance
    // needs no random-access index and must not certify skipped bad records.
    let mut digest = Sha256::new();
    let mut event_count = 0;
    let mut last_event_id = String::new();
    {
        let mut writer = BufWriter::new(&mut digest);
        writer.write_all(b"[")?;
        ledger::visit_validated(ledger_path, |event| {
            if event_count > 0 {
                writer.write_all(b",")?;
            }
            serde_json::to_writer(&mut writer, &event).map_err(io::Error::other)?;
            event_count += 1;
            last_event_id = event.id;
            Ok(())
        })?;
        writer.write_all(b"]")?;
        writer.flush()?;
    }
    let sha256 = format!("{:x}", digest.finalize());
    let state = store.state_dir(group_id)?.join("ledger/snapshots");
    fs::create_dir_all(&state)?;
    let name = format!("{}.json", stamp());
    let path = state.join(&name);
    let snapshot = LedgerSnapshot {
        v: 1,
        group_id: group_id.into(),
        created_at: utc_now(),
        reason: reason.into(),
        event_count,
        last_event_id,
        sha256,
        path: format!("state/ledger/snapshots/{name}"),
    };
    write_json(&path, &snapshot)?;
    write_json(
        &store
            .state_dir(group_id)?
            .join("ledger/snapshot.latest.json"),
        &snapshot,
    )?;
    Ok(snapshot)
}

pub fn compact(home: &HomeLayout, group_id: &str, reason: &str) -> io::Result<Option<PathBuf>> {
    let store = GroupStore::new(home.clone())?;
    let active = store.ledger_path(group_id)?;
    let lock = ledger::acquire_writer_lock(&active)?;
    let result = (|| {
        if !active.exists() || active.metadata()?.len() == 0 {
            return Ok(None);
        }
        snapshot_locked(&store, group_id, reason, &active)?;
        let state = store.state_dir(group_id)?.join("ledger");
        let segments = state.join("segments");
        fs::create_dir_all(&segments)?;
        let manifest_path = state.join("manifest.json");
        let mut manifest = reconcile_manifest(load_manifest(&manifest_path)?, &segments)?;
        let sequence = manifest
            .get("next_segment_seq")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1);
        let created_at = stamp();
        let name = format!("ledger.{created_at}.{sequence:06}.jsonl");
        let destination = segments.join(&name);
        fs::rename(&active, &destination)?;
        fs::File::create(&active)?.sync_all()?;
        let size = destination.metadata()?.len();
        let line_count = BufReader::new(fs::File::open(&destination)?)
            .lines()
            .try_fold(0_u64, |count, line| line.map(|_| count + 1))?;
        let segment = json!({
            "id":format!("{sequence:06}"),
            "seq":sequence,
            "path":format!("state/ledger/segments/{name}"),
            "compressed":false,
            "created_at":created_at,
            "sealed_at":created_at,
            "reason":reason,
            "size_bytes":size,
            "line_count":line_count,
        });
        manifest["schema"] = json!(1);
        manifest["active"] = json!({"path":"ledger.jsonl"});
        manifest["next_segment_seq"] = json!(sequence + 1);
        manifest["updated_at"] = json!(created_at);
        if !manifest["segments"].is_array() {
            manifest["segments"] = json!([]);
        }
        manifest["segments"]
            .as_array_mut()
            .expect("segments initialized")
            .push(segment);
        write_json(&manifest_path, &manifest)?;
        Ok(Some(destination))
    })();
    let _ = FileExt::unlock(&lock);
    result
}

#[cfg(not(target_os = "linux"))]
pub fn zstd_preflight() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "low-priority zstd worker is unavailable on this platform",
    ))
}

pub type GroupWriteCoordinator<'a> =
    dyn FnMut(&mut dyn FnMut() -> io::Result<()>) -> io::Result<()> + Send + 'a;

#[cfg(not(target_os = "linux"))]
pub fn compress_sealed_coordinated(
    _: &Path,
    _: &Path,
    _: &Path,
    _: &same_file::Handle,
    _: &same_file::Handle,
    _: &mut GroupWriteCoordinator<'_>,
) -> io::Result<()> {
    zstd_preflight()
}

#[cfg(target_os = "linux")]
pub fn zstd_preflight() -> io::Result<()> {
    for command in [
        "/usr/bin/ionice",
        "/usr/bin/nice",
        "/usr/bin/renice",
        "/usr/bin/prlimit",
        "/usr/bin/zstd",
    ] {
        if !fs::metadata(command)?.is_file() {
            return Err(io::Error::other("low-priority zstd worker is unavailable"));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn bounded_digest(
    reader: &mut impl Read,
    limit: u64,
    deadline: Instant,
) -> io::Result<(u64, String)> {
    let mut hash = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "digest timed out"));
        }
        let remaining = limit.saturating_add(1).saturating_sub(size);
        let chunk = remaining.min(buffer.len() as u64) as usize;
        let count = reader.read(&mut buffer[..chunk])?;
        if count == 0 {
            break;
        }
        size += count as u64;
        if size > limit {
            return Err(io::Error::other("digest input exceeds worker bounds"));
        }
        hash.update(&buffer[..count]);
    }
    Ok((size, format!("{:x}", hash.finalize())))
}

#[cfg(target_os = "linux")]
fn source_digest(file: &mut fs::File, limit: u64, deadline: Instant) -> io::Result<(u64, String)> {
    file.seek(SeekFrom::Start(0))?;
    bounded_digest(file, limit, deadline)
}

#[cfg(target_os = "linux")]
struct ZstdWorker(std::process::Child);

#[cfg(target_os = "linux")]
impl Drop for ZstdWorker {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[cfg(target_os = "linux")]
pub fn compress_sealed_coordinated(
    active: &Path,
    segment: &Path,
    group: &Path,
    expected_group: &same_file::Handle,
    expected_segment: &same_file::Handle,
    with_group_write: &mut GroupWriteCoordinator<'_>,
) -> io::Result<()> {
    // Priority is per Linux thread. Use a dedicated thread so that reducing
    // its priority never contaminates a reused daemon blocking-pool thread;
    // its feeder and compressor inherit the same low-priority scheduling.
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                set_worker_priority()?;
                compress_sealed_inner(
                    active,
                    segment,
                    group,
                    expected_group,
                    expected_segment,
                    with_group_write,
                )
            })
            .join()
            .map_err(|_| io::Error::other("sealed history worker panicked"))?
    })
}

#[cfg(target_os = "linux")]
fn set_worker_priority() -> io::Result<()> {
    // Linux accepts a thread id in both commands. Resolve it inside this
    // dedicated worker, not in the daemon or a reused blocking-pool thread.
    let thread = fs::read_link("/proc/thread-self")?;
    let tid = thread
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .ok_or_else(|| io::Error::other("worker thread identity unavailable"))?;
    for (program, args) in [
        ("/usr/bin/renice", vec!["-n", "19", "-p", tid]),
        ("/usr/bin/ionice", vec!["-c", "3", "-p", tid]),
    ] {
        let status = Command::new(program)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            return Err(io::Error::other("worker priority could not be enforced"));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn compress_sealed_inner(
    active: &Path,
    segment: &Path,
    group: &Path,
    expected_group: &same_file::Handle,
    expected_segment: &same_file::Handle,
    with_group_write: &mut GroupWriteCoordinator<'_>,
) -> io::Result<()> {
    const MAX_INPUT: u64 = 512 * 1024 * 1024;
    const MAX_OUTPUT: u64 = 768 * 1024 * 1024;
    const TIMEOUT: Duration = Duration::from_secs(1800);
    let deadline = Instant::now() + TIMEOUT;
    let directory = segment
        .parent()
        .ok_or_else(|| io::Error::other("segment has no parent"))?;
    // Pin both the Group and the sealed member under the daemon's short
    // Group write permit; no long-running work holds that permit.
    let mut prepared = None;
    with_group_write(&mut || {
        if same_file::Handle::from_path(group)? != *expected_group {
            return Err(io::Error::other("group changed before compression"));
        }
        let compressor = fs::File::open(directory)?;
        FileExt::try_lock_exclusive(&compressor)?;
        for entry in fs::read_dir(directory)? {
            if entry?
                .file_name()
                .to_string_lossy()
                .starts_with(".cccc-zstd-")
            {
                return Err(io::Error::other("unreconciled zstd staging file"));
            }
        }
        let _writer = ledger::acquire_writer_lock(active)?;
        if !fs::symlink_metadata(segment)?.file_type().is_file() {
            return Err(io::Error::other("sealed source is not a regular file"));
        }
        let source = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(segment)?;
        let metadata = source.metadata()?;
        if metadata.len() > MAX_INPUT {
            return Err(io::Error::other("sealed source exceeds worker bounds"));
        }
        let identity = same_file::Handle::from_file(source.try_clone()?)?;
        if identity != *expected_segment {
            return Err(io::Error::other("sealed source changed before compression"));
        }
        let temp = tempfile::Builder::new()
            .prefix(".cccc-zstd-")
            .tempfile_in(directory)?;
        // Keep the staging name on errors. A later run holds rather than
        // deleting a path that might belong to a replaced Group directory.
        let (temp, temp_path) = temp.keep().map_err(|error| error.error)?;
        prepared = Some((compressor, source, metadata, identity, temp, temp_path));
        Ok(())
    })?;
    let (_compressor, mut source, metadata, identity, temp, temp_path) =
        prepared.ok_or_else(|| io::Error::other("group coordinator did not prepare"))?;
    let (size, expected) = source_digest(&mut source, MAX_INPUT, deadline)?;
    let after_hash = source.metadata()?;
    if size != metadata.len()
        || after_hash.modified().ok() != metadata.modified().ok()
        || (after_hash.ctime(), after_hash.ctime_nsec())
            != (metadata.ctime(), metadata.ctime_nsec())
    {
        return Err(io::Error::other("sealed source changed before compression"));
    }
    source.seek(SeekFrom::Start(0))?;

    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "zstd worker timed out",
        ));
    }
    let output = temp.try_clone()?;
    let input = source.try_clone()?;
    let mut child = ZstdWorker(
        Command::new("/usr/bin/ionice")
            .args(["-c", "3", "/usr/bin/nice", "-n", "19", "/usr/bin/prlimit"])
            .args([
                "--fsize=805306368:805306368",
                "--as=1073741824:1073741824",
                "--cpu=1800:1800",
                "--",
            ])
            .args(["/usr/bin/zstd", "--single-thread", "-11", "-q", "-c"])
            .stdin(Stdio::piped())
            .stdout(Stdio::from(output))
            .stderr(Stdio::null())
            .spawn()?,
    );
    let mut stdin = child
        .0
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("zstd input missing"))?;
    let feeder = std::thread::Builder::new()
        .name("ledger-zstd-input".into())
        .spawn(move || io::copy(&mut input.take(size), &mut stdin))?;
    let result = loop {
        match child.0.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(error) => break Err(error),
        }
        if Instant::now() >= deadline {
            break Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "zstd worker timed out",
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    if result.is_err() {
        let _ = child.0.kill();
        let _ = child.0.wait();
    }
    let copied = feeder
        .join()
        .map_err(|_| io::Error::other("zstd input worker failed"))??;
    let status = result?;
    if copied != size {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "sealed source changed during compression",
        ));
    }
    if !status.success() || temp.metadata()?.len() > MAX_OUTPUT {
        return Err(io::Error::other(
            "zstd worker failed or exceeded output limit",
        ));
    }
    temp.sync_all()?;
    let mut decode_file = temp.try_clone()?;
    decode_file.seek(SeekFrom::Start(0))?;
    let mut decoded = zstd::stream::read::Decoder::new(decode_file)?;
    decoded.window_log_max(27)?;
    if bounded_digest(&mut decoded, size, deadline)? != (size, expected.clone()) {
        return Err(io::Error::other(
            "zstd verification differs from sealed source",
        ));
    }

    if source_digest(&mut source, MAX_INPUT, deadline)? != (size, expected.clone()) {
        return Err(io::Error::other(
            "sealed source changed during verification",
        ));
    }
    let mut archive = temp.try_clone()?;
    let (archive_len, archive_sha) = source_digest(&mut archive, MAX_OUTPUT, deadline)?;
    let archive_identity = same_file::Handle::from_file(temp.try_clone()?)?;
    let archive_meta = temp.metadata()?;
    if archive_meta.len() != archive_len {
        return Err(io::Error::other(
            "staged archive changed during verification",
        ));
    }
    let target = segment.with_extension("jsonl.zst");
    let original_name = segment
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::other("invalid segment name"))?;
    let target_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::other("invalid archive name"))?;
    let manifest = directory
        .parent()
        .ok_or_else(|| io::Error::other("segment state absent"))?
        .join("manifest.json");
    let original_path = format!("state/ledger/segments/{original_name}");
    with_group_write(&mut || {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "zstd worker timed out",
            ));
        }
        if same_file::Handle::from_path(group)? != *expected_group {
            return Err(io::Error::other("group changed before publication"));
        }
        let _writer = ledger::acquire_writer_lock(active)?;
        let current = fs::symlink_metadata(segment)?;
        if !current.file_type().is_file()
            || current.len() != metadata.len()
            || current.modified().ok() != metadata.modified().ok()
            || (current.ctime(), current.ctime_nsec()) != (metadata.ctime(), metadata.ctime_nsec())
            || same_file::Handle::from_path(segment)? != identity
        {
            return Err(io::Error::other("sealed source changed before publication"));
        }
        let initial = load_manifest(&manifest)?;
        let entries = initial["segments"]
            .as_array()
            .ok_or_else(|| io::Error::other("manifest segments absent"))?;
        if entries
            .iter()
            .filter(|record| {
                record.get("path").and_then(Value::as_str) == Some(original_path.as_str())
            })
            .count()
            != 1
        {
            return Err(io::Error::other("manifest does not bind the sealed source"));
        }
        let before_link = temp.metadata()?;
        if before_link.len() != archive_meta.len()
            || (before_link.ctime(), before_link.ctime_nsec())
                != (archive_meta.ctime(), archive_meta.ctime_nsec())
            || same_file::Handle::from_path(&temp_path)? != archive_identity
        {
            return Err(io::Error::other(
                "staged archive changed before publication",
            ));
        }
        fs::hard_link(&temp_path, &target)?;
        if same_file::Handle::from_path(&target)? != archive_identity {
            return Err(io::Error::other("published archive identity is unverified"));
        }
        crate::fs::sync_dir(directory)?;
        Ok(())
    })?;
    // An uncommitted archive is ignored by readers. Hash it outside the
    // writer lock, then revalidate the identities before committing metadata.
    let mut linked = temp.try_clone()?;
    let linked_before = linked.metadata()?;
    if same_file::Handle::from_file(linked.try_clone()?)? != archive_identity
        || source_digest(&mut linked, MAX_OUTPUT, deadline)? != (archive_len, archive_sha.clone())
    {
        return Err(io::Error::other("published archive digest differs"));
    }
    let linked_after = linked.metadata()?;
    if (linked_before.ctime(), linked_before.ctime_nsec())
        != (linked_after.ctime(), linked_after.ctime_nsec())
    {
        return Err(io::Error::other(
            "published archive changed during validation",
        ));
    }
    with_group_write(&mut || {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "zstd worker timed out",
            ));
        }
        if same_file::Handle::from_path(group)? != *expected_group {
            return Err(io::Error::other("group changed before manifest commit"));
        }
        let _writer = ledger::acquire_writer_lock(active)?;
        let current = fs::symlink_metadata(segment)?;
        let final_archive = fs::symlink_metadata(&target)?;
        if !current.file_type().is_file()
            || !final_archive.file_type().is_file()
            || current.len() != metadata.len()
            || (current.ctime(), current.ctime_nsec()) != (metadata.ctime(), metadata.ctime_nsec())
            || same_file::Handle::from_path(segment)? != identity
            || same_file::Handle::from_path(&target)? != archive_identity
            || final_archive.len() != linked_after.len()
            || (final_archive.ctime(), final_archive.ctime_nsec())
                != (linked_after.ctime(), linked_after.ctime_nsec())
        {
            return Err(io::Error::other(
                "source or archive changed before manifest commit",
            ));
        }
        let mut state = load_manifest(&manifest)?;
        let records = state["segments"]
            .as_array_mut()
            .ok_or_else(|| io::Error::other("manifest segments absent"))?;
        let mut matches = records.iter_mut().filter(|record| {
            record.get("path").and_then(Value::as_str) == Some(original_path.as_str())
        });
        let first = matches
            .next()
            .ok_or_else(|| io::Error::other("manifest no longer binds the sealed source"))?;
        if matches.next().is_some() {
            return Err(io::Error::other(
                "manifest binds the sealed source more than once",
            ));
        }
        let record = first
            .as_object_mut()
            .ok_or_else(|| io::Error::other("invalid segment record"))?;
        record.insert(
            "path".into(),
            json!(format!("state/ledger/segments/{target_name}")),
        );
        record.insert("compressed".into(), json!(true));
        record.insert("size_bytes".into(), json!(archive_len));
        record.insert("source_sha256".into(), json!(expected.clone()));
        record.insert("archive_sha256".into(), json!(archive_sha.clone()));
        write_json(&manifest, &state)?;
        if same_file::Handle::from_path(&temp_path)? != archive_identity {
            return Err(io::Error::other(
                "staging name changed after manifest commit",
            ));
        }
        fs::remove_file(&temp_path)?;
        crate::fs::sync_dir(directory)?;
        Ok(())
    })
}

fn stamp() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

fn load_manifest(path: &std::path::Path) -> io::Result<Value> {
    if path.exists() {
        let manifest: Value = read_json(path)?;
        if manifest.is_object() {
            return Ok(manifest);
        }
        return Err(io::Error::other("ledger manifest must be an object"));
    }
    Ok(json!({
        "schema":1,
        "active":{"path":"ledger.jsonl"},
        "next_segment_seq":1,
        "segments":[],
        "updated_at":"",
    }))
}

#[derive(Debug)]
struct PhysicalSegment {
    sequence: u64,
    stamp: String,
    path: PathBuf,
    relative_path: String,
    compressed: bool,
    size_bytes: u64,
    line_count: u64,
}

fn reconcile_manifest(mut manifest: Value, segments_dir: &Path) -> io::Result<Value> {
    let discovered = discover_segments(segments_dir)?;
    let existing = manifest["segments"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            item.get("seq")
                .and_then(Value::as_u64)
                .filter(|sequence| *sequence > 0)
                .map(|sequence| (sequence, item.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    let mut reconciled = Vec::with_capacity(discovered.len());
    for segment in discovered.values() {
        let mut item = existing
            .get(&segment.sequence)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        item.insert("id".into(), json!(format!("{:06}", segment.sequence)));
        item.insert("seq".into(), json!(segment.sequence));
        item.insert("path".into(), json!(segment.relative_path));
        item.insert("compressed".into(), json!(segment.compressed));
        item.insert("size_bytes".into(), json!(segment.size_bytes));
        item.insert("line_count".into(), json!(segment.line_count));
        if item
            .get("created_at")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            item.insert("created_at".into(), json!(segment.stamp));
        }
        if item
            .get("sealed_at")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            item.insert("sealed_at".into(), json!(segment.stamp));
        }
        if item
            .get("reason")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            item.insert("reason".into(), json!("recovered"));
        }
        reconciled.push(Value::Object(item));
    }
    let physical_next = discovered
        .keys()
        .next_back()
        .copied()
        .unwrap_or(0)
        .saturating_add(1)
        .max(1);
    let manifested_next = manifest
        .get("next_segment_seq")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1);
    manifest["schema"] = json!(1);
    manifest["active"] = json!({"path":"ledger.jsonl"});
    manifest["next_segment_seq"] = json!(manifested_next.max(physical_next));
    manifest["segments"] = Value::Array(reconciled);
    Ok(manifest)
}

fn discover_segments(segments_dir: &Path) -> io::Result<BTreeMap<u64, PhysicalSegment>> {
    let mut logical = BTreeMap::<String, PhysicalSegment>::new();
    let mut uncommitted = BTreeSet::new();
    if !segments_dir.is_dir() {
        return Ok(BTreeMap::new());
    }
    for entry in fs::read_dir(segments_dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let Some((stamp, sequence, compressed, logical_name)) = parse_segment_name(name) else {
            continue;
        };
        if !fs::symlink_metadata(&path)?.file_type().is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ledger segment is not a regular file",
            ));
        }
        if name.ends_with(".zst") && ledger::zstd_binding(&path)?.is_none() {
            // The original remains authoritative after a crash before the
            // manifest commit. Never adopt an orphan automatically.
            uncommitted.insert(logical_name);
            continue;
        }
        let relative_path = format!("state/ledger/segments/{name}");
        let candidate = PhysicalSegment {
            sequence,
            stamp,
            size_bytes: path.metadata()?.len(),
            line_count: count_segment_lines(&path)?,
            path,
            relative_path,
            compressed,
        };
        let replace = logical
            .get(&logical_name)
            .is_none_or(|current| segment_rank(&candidate.path) > segment_rank(&current.path));
        if replace {
            logical.insert(logical_name, candidate);
        }
    }
    if uncommitted.iter().any(|name| !logical.contains_key(name)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "uncommitted zstd segment has no readable source",
        ));
    }
    let mut by_sequence = BTreeMap::new();
    for segment in logical.into_values() {
        if let Some(previous) = by_sequence.insert(segment.sequence, segment) {
            let current = by_sequence
                .get(&previous.sequence)
                .expect("inserted segment exists");
            return Err(io::Error::other(format!(
                "ambiguous ledger segments share sequence {:06}: {} and {}",
                previous.sequence,
                previous.path.display(),
                current.path.display()
            )));
        }
    }
    Ok(by_sequence)
}

fn parse_segment_name(name: &str) -> Option<(String, u64, bool, String)> {
    let (without_compression, compressed) = name
        .strip_suffix(".zst")
        .or_else(|| name.strip_suffix(".gz"))
        .map_or((name, false), |value| (value, true));
    let logical_name = without_compression.to_owned();
    let stem = without_compression.strip_suffix(".jsonl")?;
    let body = stem.strip_prefix("ledger.")?;
    let (stamp, sequence) = body.rsplit_once('.')?;
    if stamp.is_empty()
        || sequence.len() != 6
        || !sequence.bytes().all(|value| value.is_ascii_digit())
    {
        return None;
    }
    let sequence = sequence.parse().ok()?;
    if sequence == 0 {
        return None;
    }
    Some((stamp.to_owned(), sequence, compressed, logical_name))
}

fn segment_rank(path: &Path) -> u8 {
    match path.extension().and_then(|value| value.to_str()) {
        Some("zst") => 2,
        Some("gz") => 1,
        _ => 0,
    }
}

fn count_segment_lines(path: &Path) -> io::Result<u64> {
    ledger::open_source_reader(path)?
        .lines()
        .try_fold(0_u64, |count, line| line.map(|_| count + 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cccc_contracts::Event;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[cfg(target_os = "linux")]
    #[test]
    fn dedicated_worker_observes_idle_io_and_low_cpu_priority() {
        std::thread::spawn(|| {
            set_worker_priority().expect("priority must be enforceable");
            let stat = fs::read_to_string("/proc/thread-self/stat").expect("thread stat");
            let fields = stat.rsplit_once(") ").expect("stat comm").1;
            assert_eq!(fields.split_whitespace().nth(16), Some("19"));
            let tid = fs::read_link("/proc/thread-self").expect("thread identity");
            let tid = tid.file_name().expect("tid");
            let result = Command::new("/usr/bin/ionice")
                .arg("-p")
                .arg(tid)
                .output()
                .expect("ionice observation");
            assert!(result.status.success());
            assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "idle");
        })
        .join()
        .expect("worker thread");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn captured_digest_rejects_input_over_its_bound() {
        const CAPTURE: &[u8] = include_bytes!("../tests/fixtures/ledger-zstd-redacted.jsonl");
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("captured.jsonl");
        fs::write(&source, CAPTURE).expect("capture");
        let mut file = fs::File::open(&source).expect("open");
        let limit = u64::try_from(CAPTURE.len()).expect("length");
        assert_eq!(
            source_digest(&mut file, limit, Instant::now() + Duration::from_secs(10))
                .expect("bounded digest"),
            (limit, format!("{:x}", Sha256::digest(CAPTURE)))
        );
        // Labeled limit mutation: the same real bytes exceed this ceiling.
        assert!(
            source_digest(
                &mut file,
                limit - 1,
                Instant::now() + Duration::from_secs(10)
            )
            .is_err()
        );
        assert_eq!(
            source_digest(&mut file, limit, Instant::now() - Duration::from_secs(1))
                .expect_err("expired deadline")
                .kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn manifest_reconciliation_selects_one_zstd_representation() {
        const CAPTURE: &[u8] = include_bytes!("../tests/fixtures/ledger-zstd-redacted.jsonl");
        let temp = tempfile::tempdir().expect("tempdir");
        let name = "ledger.20261103T000000Z.000001.jsonl";
        let segments = temp.path().join("segments");
        fs::create_dir(&segments).expect("segments");
        let plain = segments.join(name);
        fs::write(&plain, CAPTURE).expect("capture");
        let mut encoded = zstd::stream::write::Encoder::new(
            fs::File::create(plain.with_extension("jsonl.zst")).expect("zstd"),
            11,
        )
        .expect("encoder");
        encoded.include_checksum(true).expect("checksum");
        encoded.write_all(CAPTURE).expect("capture");
        encoded.finish().expect("finish");
        // The source directory above stands in for the ledger state in this
        // control. Bind the generated frame to the captured source bytes.
        let archive_sha = format!(
            "{:x}",
            Sha256::digest(fs::read(plain.with_extension("jsonl.zst")).expect("archive"))
        );
        let source_sha = format!("{:x}", Sha256::digest(CAPTURE));
        let binding = json!({"segments": [{
            "path":format!("state/ledger/segments/{name}.zst"),
            "seq":1, "compressed":true, "archive_sha256":archive_sha, "source_sha256":source_sha
        }]});
        write_json(&temp.path().join("manifest.json"), &binding).expect("bind archive");
        let manifest = reconcile_manifest(binding, &segments).expect("reconcile captured history");
        let records = manifest["segments"].as_array().expect("segments");
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0]["path"],
            format!("state/ledger/segments/{name}.zst")
        );
        assert_eq!(records[0]["line_count"], 64);
        assert_eq!(fs::read(&plain).expect("retained source"), CAPTURE);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_worker_retains_the_captured_sealed_source() {
        const CAPTURE: &[u8] = include_bytes!("../tests/fixtures/ledger-zstd-redacted.jsonl");
        let temp = tempfile::tempdir().expect("tempdir");
        let group = temp.path().join("g_example");
        let segments = group.join("state/ledger/segments");
        fs::create_dir_all(&segments).expect("segments");
        let active = group.join("ledger.jsonl");
        fs::write(&active, b"").expect("active");
        let plain = segments.join("ledger.20261103T000000Z.000001.jsonl");
        fs::write(&plain, CAPTURE).expect("sealed capture");
        let manifest = segments
            .parent()
            .expect("ledger state")
            .join("manifest.json");
        let initial = reconcile_manifest(
            load_manifest(&manifest).expect("default manifest"),
            &segments,
        )
        .expect("reconcile captured source");
        write_json(&manifest, &initial).expect("record captured source");
        let group_identity = same_file::Handle::from_path(&group).expect("group identity");
        let segment_identity = same_file::Handle::from_path(&plain).expect("segment identity");
        let mut guard = |work: &mut dyn FnMut() -> io::Result<()>| work();
        compress_sealed_coordinated(
            &active,
            &plain,
            &group,
            &group_identity,
            &segment_identity,
            &mut guard,
        )
        .expect("compress capture");
        let compressed = plain.with_extension("jsonl.zst");
        let mut decoder = zstd::stream::read::Decoder::new(
            fs::File::open(&compressed).expect("published archive"),
        )
        .expect("decoder");
        let mut restored = Vec::new();
        decoder.read_to_end(&mut restored).expect("decode");
        assert_eq!(restored, CAPTURE);
        assert_eq!(fs::read(&plain).expect("retained original"), CAPTURE);
        let before = fs::read(&compressed).expect("archive");
        assert!(
            compress_sealed_coordinated(
                &active,
                &plain,
                &group,
                &group_identity,
                &segment_identity,
                &mut guard
            )
            .is_err()
        );
        assert_eq!(fs::read(&compressed).expect("unchanged archive"), before);
        assert_eq!(fs::read(&plain).expect("retained original"), CAPTURE);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn interrupted_publication_keeps_captured_history_readable_and_holds_retry() {
        const CAPTURE: &[u8] = include_bytes!("../tests/fixtures/ledger-zstd-redacted.jsonl");
        let temp = tempfile::tempdir().expect("tempdir");
        let group = temp.path().join("g_example");
        let segments = group.join("state/ledger/segments");
        fs::create_dir_all(&segments).expect("segments");
        let active = group.join("ledger.jsonl");
        let plain = segments.join("ledger.20261103T000000Z.000001.jsonl");
        let target = plain.with_extension("jsonl.zst");
        fs::write(&active, b"").expect("active");
        fs::write(&plain, CAPTURE).expect("captured source");
        let manifest = segments
            .parent()
            .expect("ledger state")
            .join("manifest.json");
        let initial = reconcile_manifest(load_manifest(&manifest).expect("default"), &segments)
            .expect("captured source");
        write_json(&manifest, &initial).expect("manifest");
        let before = ledger::read_all_uncached(&active).expect("original history");
        let group_identity = same_file::Handle::from_path(&group).expect("group identity");
        let segment_identity = same_file::Handle::from_path(&plain).expect("segment identity");

        // Labeled interruption after the archive link is durable but before
        // its manifest binding. The actual crash proof is a separate gate.
        let mut calls = 0;
        let mut interrupted = |work: &mut dyn FnMut() -> io::Result<()>| {
            calls += 1;
            work()?;
            if calls == 2 {
                return Err(io::Error::other("injected interruption after link"));
            }
            Ok(())
        };
        assert!(
            compress_sealed_coordinated(
                &active,
                &plain,
                &group,
                &group_identity,
                &segment_identity,
                &mut interrupted,
            )
            .is_err()
        );
        assert_eq!(calls, 2);
        assert!(target.is_file());
        assert_eq!(fs::read(&plain).expect("original retained"), CAPTURE);
        assert_eq!(
            ledger::read_all_uncached(&active).expect("recovered history"),
            before
        );
        let recovered = reconcile_manifest(load_manifest(&manifest).expect("manifest"), &segments)
            .expect("reconcile");
        assert_eq!(
            recovered["segments"][0]["path"],
            initial["segments"][0]["path"]
        );
        let mut guard = |work: &mut dyn FnMut() -> io::Result<()>| work();
        assert!(
            compress_sealed_coordinated(
                &active,
                &plain,
                &group,
                &group_identity,
                &segment_identity,
                &mut guard,
            )
            .is_err()
        );
        assert_eq!(
            ledger::read_all_uncached(&active).expect("held history"),
            before
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn replaced_group_is_refused_before_staging_a_captured_segment() {
        const CAPTURE: &[u8] = include_bytes!("../tests/fixtures/ledger-zstd-redacted.jsonl");
        let temp = tempfile::tempdir().expect("tempdir");
        let group = temp.path().join("g_example");
        let segments = group.join("state/ledger/segments");
        fs::create_dir_all(&segments).expect("segments");
        let active = group.join("ledger.jsonl");
        let plain = segments.join("ledger.20261103T000000Z.000001.jsonl");
        fs::write(&active, b"").expect("active");
        fs::write(&plain, CAPTURE).expect("capture");
        let group_identity = same_file::Handle::from_path(&group).expect("group identity");
        let segment_identity = same_file::Handle::from_path(&plain).expect("segment identity");
        // Labeled namespace replacement: both copies contain the same capture.
        fs::rename(&group, temp.path().join("retained")).expect("retain original group");
        fs::create_dir_all(&segments).expect("replacement segments");
        fs::write(&active, b"").expect("replacement active");
        fs::write(&plain, CAPTURE).expect("replacement capture");
        let mut guard = |work: &mut dyn FnMut() -> io::Result<()>| work();
        assert!(
            compress_sealed_coordinated(
                &active,
                &plain,
                &group,
                &group_identity,
                &segment_identity,
                &mut guard
            )
            .is_err()
        );
        assert_eq!(fs::read(&plain).expect("replacement unchanged"), CAPTURE);
        assert_eq!(fs::read_dir(&segments).expect("segments").count(), 1);
    }

    #[test]
    fn snapshot_does_not_populate_the_query_index() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path()).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("streaming snapshot", "").expect("group");
        let path = store.ledger_path(&group.group_id).expect("ledger");
        let event = Event::new("chat.message", &group.group_id);
        ledger::append(&path, &event).expect("append");
        crate::ledger_index::invalidate_path(&path);

        let actual = snapshot(&home, &group.group_id, "fixture").expect("snapshot");
        assert_eq!(actual.last_event_id, event.id);
        assert!(
            !crate::ledger_index::is_cached(&path),
            "maintenance must not retain the entire history in a query index"
        );
    }

    #[test]
    fn snapshot_preserves_empty_and_gzip_canonical_hashes_and_default_fields() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path()).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("canonical snapshot", "").expect("group");
        let path = store.ledger_path(&group.group_id).expect("ledger");
        let empty = snapshot(&home, &group.group_id, "empty").expect("snapshot");
        assert_eq!(empty.event_count, 0);
        assert_eq!(empty.last_event_id, "");
        assert_eq!(empty.sha256, format!("{:x}", Sha256::digest(b"[]")));

        let raw = json!({
            "id":"archived", "ts":"2026-09-08T00:00:00Z", "kind":"chat.message",
            "group_id":group.group_id, "data":{"text":"历史\n\"quoted\"", "number":1.25}
        });
        let first: Event = serde_json::from_value(raw.clone()).expect("default fields");
        let segments = store
            .state_dir(&group.group_id)
            .expect("state")
            .join("ledger/segments");
        fs::create_dir_all(&segments).expect("segments");
        let archived = segments.join("ledger.20260908T000000Z.000001.jsonl.gz");
        let mut gzip = flate2::write::GzEncoder::new(
            fs::File::create(&archived).expect("archive"),
            flate2::Compression::default(),
        );
        write!(gzip, "\r\n  {raw}\r\n\t\n").expect("archived record");
        gzip.finish().expect("gzip");
        let last = Event::new("chat.message", &group.group_id);
        // A complete final JSON object without a newline remains readable.
        fs::write(&path, serde_json::to_vec(&last).expect("event")).expect("active");
        let expected = vec![first, last];
        let actual = snapshot(&home, &group.group_id, "fixture").expect("snapshot");
        assert_eq!(actual.event_count, 2);
        assert_eq!(actual.last_event_id, expected[1].id);
        assert_eq!(
            actual.sha256,
            format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&expected).expect("array"))
            )
        );

        fs::write(&archived, b"broken gzip").expect("corrupt archive");
        assert!(snapshot(&home, &group.group_id, "corrupt").is_err());
    }

    #[test]
    fn maintenance_rejects_json_objects_that_cannot_be_read_as_events() {
        for invalid in [
            json!({"kind":false,"group_id":"fixture"}),
            json!({"kind":"chat.message","group_id":"fixture","unknown":true}),
            json!([
                1,
                "id",
                "2026-09-08T00:00:00Z",
                "chat.message",
                "fixture",
                "",
                "user",
                {}
            ]),
        ] {
            let temp = tempfile::tempdir().expect("tempdir");
            let home = HomeLayout::from_path(temp.path()).expect("home");
            let store = GroupStore::new(home.clone()).expect("store");
            let group = store.create("invalid event", "").expect("group");
            let path = store.ledger_path(&group.group_id).expect("ledger");
            writeln!(
                fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .expect("ledger"),
                "{invalid}"
            )
            .expect("invalid event fixture");
            let original = fs::read(&path).expect("original");
            assert!(snapshot(&home, &group.group_id, "fixture").is_err());
            assert!(compact(&home, &group.group_id, "fixture").is_err());
            assert_eq!(fs::read(&path).expect("unchanged ledger"), original);
            assert!(
                !store
                    .state_dir(&group.group_id)
                    .expect("state")
                    .join("ledger/snapshot.latest.json")
                    .exists()
            );
        }
    }

    #[test]
    fn maintenance_rejects_records_without_persisted_identity() {
        for field in ["id", "ts"] {
            let temp = tempfile::tempdir().expect("tempdir");
            let home = HomeLayout::from_path(temp.path()).expect("home");
            let store = GroupStore::new(home.clone()).expect("store");
            let group = store.create("missing identity", "").expect("group");
            let path = store.ledger_path(&group.group_id).expect("ledger");
            let mut value =
                serde_json::to_value(Event::new("chat.message", &group.group_id)).expect("event");
            value.as_object_mut().expect("object").remove(field);
            let original = serde_json::to_vec(&value).expect("record");
            fs::write(&path, &original).expect("fixture");
            assert!(
                snapshot(&home, &group.group_id, "missing identity").is_err(),
                "{field} must not be generated while certifying persisted history"
            );
            assert_eq!(fs::read(&path).expect("unchanged ledger"), original);
        }
    }

    #[test]
    fn snapshot_hash_matches_the_canonical_event_array_across_archives() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path()).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("snapshot hash", "").expect("group");
        let path = store.ledger_path(&group.group_id).expect("ledger");
        let mut expected = ledger::read_all(&path).expect("initial history");
        for text in ["南京天气 \"quoted\"\nsecond line", "after rotation"] {
            let mut event = Event::new("chat.message", &group.group_id);
            event.data.insert("text".into(), text.into());
            ledger::append(&path, &event).expect("append");
            expected.push(event);
            if text.starts_with("南京") {
                compact(&home, &group.group_id, "fixture").expect("compact");
            }
        }
        let actual = snapshot(&home, &group.group_id, "fixture").expect("snapshot");
        assert_eq!(actual.event_count, expected.len());
        assert_eq!(actual.last_event_id, expected.last().expect("last").id);
        assert_eq!(
            actual.sha256,
            format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&expected).expect("canonical event array"))
            )
        );
    }

    #[test]
    fn compact_writes_python_compatible_segment_and_manifest() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path()).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("compact", "").expect("group");
        ledger::append(
            &store.ledger_path(&group.group_id).expect("ledger"),
            &Event::new("chat.message", &group.group_id),
        )
        .expect("append");

        let segment = compact(&home, &group.group_id, "test")
            .expect("compact")
            .expect("segment");
        let name = segment
            .file_name()
            .and_then(|value| value.to_str())
            .expect("name");
        assert!(name.starts_with("ledger."));
        assert!(name.ends_with(".000001.jsonl"));
        let manifest: Value = read_json(
            &store
                .state_dir(&group.group_id)
                .expect("state")
                .join("ledger/manifest.json"),
        )
        .expect("manifest");
        assert_eq!(manifest["next_segment_seq"], 2);
        assert_eq!(
            manifest["segments"][0]["path"],
            format!("state/ledger/segments/{name}")
        );
        assert_eq!(
            ledger::read_all(&store.ledger_path(&group.group_id).expect("ledger"))
                .expect("events")
                .len(),
            1
        );
    }

    #[test]
    fn compact_recovers_unpublished_segment_before_allocating_sequence() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path()).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("recover compact", "").expect("group");
        let ledger_path = store.ledger_path(&group.group_id).expect("ledger");
        let first = Event::new("chat.message", &group.group_id);
        ledger::append(&ledger_path, &first).expect("first append");

        let segments = store
            .state_dir(&group.group_id)
            .expect("state")
            .join("ledger/segments");
        fs::create_dir_all(&segments).expect("segments");
        let orphan = segments.join("ledger.20260101T000000Z.000001.jsonl");
        fs::rename(&ledger_path, &orphan).expect("inject committed rotation");
        fs::File::create(&ledger_path).expect("replacement active");
        let second = Event::new("chat.message", &group.group_id);
        ledger::append(&ledger_path, &second).expect("second append");

        compact(&home, &group.group_id, "retry")
            .expect("compact")
            .expect("second segment");
        let manifest: Value = read_json(
            &store
                .state_dir(&group.group_id)
                .expect("state")
                .join("ledger/manifest.json"),
        )
        .expect("manifest");
        assert_eq!(manifest["next_segment_seq"], 3);
        assert_eq!(manifest["segments"][0]["seq"], 1);
        assert_eq!(manifest["segments"][1]["seq"], 2);
        assert_eq!(
            manifest["segments"][0]["path"],
            "state/ledger/segments/ledger.20260101T000000Z.000001.jsonl"
        );
        let events = ledger::read_all(&ledger_path).expect("events");
        assert_eq!(
            events
                .iter()
                .map(|event| event.id.as_str())
                .collect::<Vec<_>>(),
            [first.id.as_str(), second.id.as_str()]
        );
    }

    #[test]
    fn compact_rejects_ambiguous_physical_sequences_before_rotation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path()).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("ambiguous compact", "").expect("group");
        let ledger_path = store.ledger_path(&group.group_id).expect("ledger");
        let active = Event::new("chat.message", &group.group_id);
        ledger::append(&ledger_path, &active).expect("active append");

        let segments = store
            .state_dir(&group.group_id)
            .expect("state")
            .join("ledger/segments");
        fs::create_dir_all(&segments).expect("segments");
        for stamp in ["20260101T000000Z", "20260102T000000Z"] {
            let path = segments.join(format!("ledger.{stamp}.000001.jsonl"));
            let event = Event::new("chat.message", &group.group_id);
            let mut bytes = serde_json::to_vec(&event).expect("event json");
            bytes.push(b'\n');
            fs::write(path, bytes).expect("segment");
        }
        let original_active = fs::read(&ledger_path).expect("active bytes");

        let error = compact(&home, &group.group_id, "retry").expect_err("ambiguous");
        assert!(
            error
                .to_string()
                .contains("ambiguous ledger segments share sequence")
        );
        assert_eq!(
            fs::read(&ledger_path).expect("active bytes"),
            original_active
        );
    }

    #[test]
    fn maintenance_rejects_malformed_middle_record_before_rotation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path()).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("corrupt compact", "").expect("group");
        let ledger_path = store.ledger_path(&group.group_id).expect("ledger");
        ledger::append(&ledger_path, &Event::new("chat.message", &group.group_id))
            .expect("first append");
        fs::OpenOptions::new()
            .append(true)
            .open(&ledger_path)
            .expect("corruption fixture")
            .write_all(b"{\"broken\":\n")
            .expect("malformed middle record");
        ledger::append(&ledger_path, &Event::new("chat.message", &group.group_id))
            .expect("second append");
        let original = fs::read(&ledger_path).expect("active bytes");

        let snapshot_error = snapshot(&home, &group.group_id, "corrupt")
            .expect_err("snapshot must reject malformed source truth");
        assert!(snapshot_error.to_string().contains("malformed ledger JSON"));

        let compact_error = compact(&home, &group.group_id, "corrupt")
            .expect_err("compaction must reject malformed source truth");
        assert!(compact_error.to_string().contains("malformed ledger JSON"));
        assert_eq!(fs::read(&ledger_path).expect("active bytes"), original);
        let segment_dir = store
            .state_dir(&group.group_id)
            .expect("state")
            .join("ledger/segments");
        assert!(
            !segment_dir.exists()
                || fs::read_dir(segment_dir)
                    .expect("segments")
                    .next()
                    .is_none()
        );
    }

    #[test]
    fn compact_waits_for_the_writer_lock_and_snapshots_its_commit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path()).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("locked compact", "").expect("group");
        let ledger_path = store.ledger_path(&group.group_id).expect("ledger");
        ledger::append(&ledger_path, &Event::new("chat.message", &group.group_id))
            .expect("seed append");

        let lock = ledger::acquire_writer_lock(&ledger_path).expect("writer lock");
        let mut writer = fs::OpenOptions::new()
            .append(true)
            .open(&ledger_path)
            .expect("active writer");
        let writer_event = Event::new("chat.message", &group.group_id);
        let mut encoded = serde_json::to_vec(&writer_event).expect("event json");
        encoded.push(b'\n');

        let compact_home = home.clone();
        let compact_group_id = group.group_id.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            started_tx.send(()).expect("started receiver");
            compact(&compact_home, &compact_group_id, "writer-lock")
        });
        started_rx.recv().expect("compactor started");
        thread::sleep(Duration::from_millis(250));
        assert!(
            !handle.is_finished(),
            "compaction must remain blocked while the canonical writer lock is held"
        );

        writer.write_all(&encoded).expect("writer commit");
        writer.sync_all().expect("writer sync");
        drop(writer);
        FileExt::unlock(&lock).expect("writer unlock");
        drop(lock);

        handle
            .join()
            .expect("compactor thread")
            .expect("compact")
            .expect("segment");
        let state = store.state_dir(&group.group_id).expect("state");
        let snapshot: Value =
            read_json(&state.join("ledger/snapshot.latest.json")).expect("latest snapshot");
        let manifest: Value = read_json(&state.join("ledger/manifest.json")).expect("manifest");
        assert_eq!(snapshot["last_event_id"], writer_event.id);
        assert_eq!(
            manifest["segments"][0]["line_count"],
            snapshot["event_count"]
        );
    }

    #[test]
    fn snapshot_waits_for_the_writer_lock_before_validating_the_ledger() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = HomeLayout::from_path(temp.path()).expect("home");
        let store = GroupStore::new(home.clone()).expect("store");
        let group = store.create("locked snapshot", "").expect("group");
        let ledger_path = store.ledger_path(&group.group_id).expect("ledger");
        ledger::append(&ledger_path, &Event::new("chat.message", &group.group_id))
            .expect("seed append");

        let lock = ledger::acquire_writer_lock(&ledger_path).expect("writer lock");
        let mut writer = fs::OpenOptions::new()
            .append(true)
            .open(&ledger_path)
            .expect("active writer");
        let writer_event = Event::new("chat.message", &group.group_id);
        let encoded = serde_json::to_vec(&writer_event).expect("event json");
        let split = encoded.len() / 2;
        writer
            .write_all(&encoded[..split])
            .expect("partial writer append");
        writer.sync_all().expect("partial writer sync");

        let snapshot_home = home.clone();
        let snapshot_group_id = group.group_id.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            started_tx.send(()).expect("started receiver");
            snapshot(&snapshot_home, &snapshot_group_id, "writer-lock")
        });
        started_rx.recv().expect("snapshot started");
        thread::sleep(Duration::from_millis(250));
        assert!(
            !handle.is_finished(),
            "snapshot must not validate an append while its writer lock is held"
        );

        writer
            .write_all(&encoded[split..])
            .expect("finish writer append");
        writer.write_all(b"\n").expect("writer newline");
        writer.sync_all().expect("writer sync");
        drop(writer);
        FileExt::unlock(&lock).expect("writer unlock");
        drop(lock);

        let result = handle.join().expect("snapshot thread").expect("snapshot");
        assert_eq!(result.last_event_id, writer_event.id);
        assert_eq!(result.event_count, 2);
    }
}
