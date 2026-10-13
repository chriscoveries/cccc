//! Identity-gated parse cache for small state files that are re-read on every
//! tick but almost never change.
//!
//! The gate is the file's `(dev, ino, len, mtime_ns)`. Every writer in this
//! crate goes through `fs::atomic_write`, which lands a `NamedTempFile` via
//! `persist()` — a real rename, so a successful write always produces a new
//! inode and the gate opens by itself. That is what makes the cache safe here:
//! there is no in-process writer that can change the file without changing its
//! identity.
//!
//! # Contract
//!
//! This cache is correct only for a file whose every writer lands through
//! `fs::atomic_write`. That is why the entry point is named
//! `read_yaml_atomic_cached`: the guarantee is a property of the file, not of
//! this module, and the name says so at each call site.
//!
//! What is guaranteed:
//!
//! * Any write that renames (which `atomic_write` does via `temp.persist()`)
//!   yields a new inode, so it is always observed.
//! * Any write that changes length, or that lands in a different filesystem
//!   mtime tick, is observed.
//! * Callers receive clones, so a cached document can never be mutated behind
//!   another holder's back.
//!
//! What is NOT guaranteed, and is measured rather than assumed:
//!
//! * An in-place edit that preserves length *and* lands inside the same
//!   filesystem mtime tick is invisible. ext4 stamps mtime at jiffy
//!   granularity, so five rewrites inside one tick can all report the same
//!   timestamp; a same-length rewrite in that window is missed. No stat-based
//!   gate can catch this without reading the file, which is the work being
//!   avoided.
//!
//! Writers invalidate after committing. Seeding from the caller's value would
//! be unsafe if another writer replaced the file between commit and stat.
//! The residual risk is an external tool
//! rewriting the file in place; for the two documents cached here (`group.yaml`
//! and `settings.yaml`) all in-tree writers are atomic, and a hand edit is
//! followed by a process restart in practice. Callers that cannot accept this
//! must read the file directly.

use serde::Serialize;
use serde::de::DeserializeOwned;
#[cfg(unix)]
use std::collections::HashMap;
use std::io;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::{Arc, Mutex, OnceLock};
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
const MAX_ENTRIES: usize = 64;
#[cfg(unix)]
const MAX_ESTIMATED_BYTES: usize = 8 * 1024 * 1024;
#[cfg(unix)]
const MAX_FILE_BYTES: u64 = 512 * 1024;
#[cfg(unix)]
const IDLE: Duration = Duration::from_secs(300);

/// Identity of a file as of the parse that produced the cached value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FileIdentity {
    pub len: u64,
    pub mtime_ns: i128,
    pub dev: u64,
    pub ino: u64,
}

#[cfg(unix)]
impl FileIdentity {
    pub fn of(metadata: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        FileIdentity {
            len: metadata.len(),
            mtime_ns: metadata.mtime_nsec() as i128 + (metadata.mtime() as i128) * 1_000_000_000,
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }
}

#[cfg(unix)]
struct Entry {
    identity: FileIdentity,
    value: Arc<dyn std::any::Any + Send + Sync>,
    estimated_bytes: usize,
    last_used: Instant,
    // Retain the inode until eviction so repeated atomic replacements cannot
    // recycle its identity while the cached document is still live.
    _source: std::fs::File,
}

#[cfg(unix)]
fn cache() -> &'static Mutex<HashMap<PathBuf, Entry>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Entry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Drop every cached parse. Exposed for tests and for callers that have just
/// moved a whole tree into place underneath a live process.
pub fn clear() {
    #[cfg(unix)]
    let retired = cache()
        .lock()
        .ok()
        .map(|mut guard| std::mem::take(&mut *guard));
    #[cfg(unix)]
    drop(retired);
}

/// Drop the cached parse for one path, if any.
pub fn invalidate(path: &Path) {
    #[cfg(unix)]
    let retired = cache().lock().ok().and_then(|mut guard| guard.remove(path));
    #[cfg(unix)]
    drop(retired);
    #[cfg(not(unix))]
    let _ = path;
}

/// Read and parse `path`, reusing the previous parse when the file's identity is
/// unchanged. `T` must be `Clone`; the caller receives its own copy.
///
/// Only call this for a file written exclusively through `fs::atomic_write`;
/// see the module contract.
///
/// A missing file is not cached: it may appear later, and its absence is cheap
/// to re-check.
#[cfg(unix)]
pub fn read_yaml_atomic_cached<T>(path: &Path) -> io::Result<T>
where
    T: DeserializeOwned + Serialize + Clone + Send + Sync + 'static,
{
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            invalidate(path);
            return Err(error);
        }
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_FILE_BYTES {
        // Preserve the caller's existing behaviour for anything that is not a
        // plain file; do not cache a decision we would not have made anyway.
        invalidate(path);
        return crate::fs::read_yaml(path);
    }
    let identity = FileIdentity::of(&metadata);
    let cached = cache().lock().ok().and_then(|mut guard| {
        let entry = guard.get_mut(path)?;
        if entry.identity != identity {
            return None;
        }
        entry.last_used = Instant::now();
        Some(Arc::clone(&entry.value))
    });
    if let Some(cached) = cached
        && let Ok(value) = cached.downcast::<T>()
    {
        // A document clone can allocate substantially. Do it after releasing
        // the process-wide cache mutex so unrelated groups can keep reading.
        return Ok(value.as_ref().clone());
    }
    let source = std::fs::File::open(path)?;
    let parsed_identity = FileIdentity::of(&source.metadata()?);
    let value: T =
        serde_yaml::from_reader(std::io::BufReader::new(&source)).map_err(io::Error::other)?;
    // Bind the parse only if the file still has the identity observed before
    // reading. A racing atomic replacement is returned as an ordinary read,
    // but must not seed a different file's cache entry.
    if std::fs::symlink_metadata(path).is_ok_and(|next| {
        next.is_file()
            && !next.file_type().is_symlink()
            && next.len() <= MAX_FILE_BYTES
            && FileIdentity::of(&next) == parsed_identity
    }) {
        insert(path, parsed_identity, &value, source);
    }
    Ok(value)
}

#[cfg(not(unix))]
pub fn read_yaml_atomic_cached<T>(path: &Path) -> io::Result<T>
where
    T: DeserializeOwned + Serialize + Clone + Send + Sync + 'static,
{
    // Without a replacement-safe file identity, preserve uncached semantics.
    crate::fs::read_yaml(path)
}

#[cfg(unix)]
fn insert<T>(path: &Path, identity: FileIdentity, value: &T, source: std::fs::File)
where
    T: Serialize + Clone + Send + Sync + 'static,
{
    // Normalize before estimating: YAML aliases can expand far beyond the
    // on-disk length. This is a conservative weight, plus a hard entry count,
    // rather than a claim to measure the generic T's heap exactly.
    let Ok(serialized) = serde_yaml::to_string(value) else {
        invalidate(path);
        return;
    };
    let estimated_bytes = serialized
        .len()
        .saturating_mul(16)
        .saturating_add(std::mem::size_of::<T>());
    if estimated_bytes > MAX_ESTIMATED_BYTES {
        invalidate(path);
        return;
    }
    let entry = Entry {
        identity,
        value: Arc::new(value.clone()),
        estimated_bytes,
        last_used: Instant::now(),
        _source: source,
    };
    let mut retired = Vec::new();
    if let Ok(mut guard) = cache().lock() {
        if let Some(previous) = guard.insert(path.to_path_buf(), entry) {
            retired.push(previous);
        }
        let stale = guard
            .iter()
            .filter(|(key, cached)| key.as_path() != path && cached.last_used.elapsed() >= IDLE)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in stale {
            if let Some(previous) = guard.remove(&key) {
                retired.push(previous);
            }
        }
        while guard.len() > MAX_ENTRIES
            || guard
                .values()
                .map(|cached| cached.estimated_bytes)
                .sum::<usize>()
                > MAX_ESTIMATED_BYTES
        {
            let Some(key) = guard
                .iter()
                .min_by_key(|(_, cached)| cached.last_used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(previous) = guard.remove(&key) {
                retired.push(previous);
            }
        }
    }
    drop(retired);
}

/// Invalidate after a write. The supplied value cannot safely be associated
/// with a subsequent stat: another writer may already have replaced it.
pub fn store_written<T>(path: &Path, _value: &T)
where
    T: Clone + Send + Sync + 'static,
{
    invalidate(path);
}

/// Convenience for writers: a serialized document that has just been committed.
pub fn store_serialized<T>(path: &Path, value: &T)
where
    T: Serialize + Clone + Send + Sync + 'static,
{
    store_written(path, value);
}

#[cfg(test)]
#[path = "parse_cache_tests.rs"]
mod tests;
