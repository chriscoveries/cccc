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
//! In-process writes are covered because every writer in this crate calls
//! `store_serialized` after committing. The residual risk is an external tool
//! rewriting the file in place; for the two documents cached here (`group.yaml`
//! and `settings.yaml`) all in-tree writers are atomic, and a hand edit is
//! followed by a process restart in practice. Callers that cannot accept this
//! must read the file directly.

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::OnceLock;

/// Identity of a file as of the parse that produced the cached value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FileIdentity {
    pub len: u64,
    pub mtime_ns: i128,
    pub dev: u64,
    pub ino: u64,
}

impl FileIdentity {
    pub fn of(metadata: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        FileIdentity {
            len: metadata.len(),
            mtime_ns: metadata.mtime_nsec() as i128
                + (metadata.mtime() as i128) * 1_000_000_000,
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }
}

struct Entry {
    identity: FileIdentity,
    value: Box<dyn std::any::Any + Send + Sync>,
}

fn cache() -> &'static Mutex<HashMap<PathBuf, Entry>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Entry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Drop every cached parse. Exposed for tests and for callers that have just
/// moved a whole tree into place underneath a live process.
pub fn clear() {
    if let Ok(mut guard) = cache().lock() {
        guard.clear();
    }
}

/// Drop the cached parse for one path, if any.
pub fn invalidate(path: &Path) {
    if let Ok(mut guard) = cache().lock() {
        guard.remove(path);
    }
}

/// Read and parse `path`, reusing the previous parse when the file's identity is
/// unchanged. `T` must be `Clone`; the caller receives its own copy.
///
/// Only call this for a file written exclusively through `fs::atomic_write`;
/// see the module contract.
///
/// A missing file is not cached: it may appear later, and its absence is cheap
/// to re-check.
pub fn read_yaml_atomic_cached<T>(path: &Path) -> io::Result<T>
where
    T: DeserializeOwned + Clone + Send + Sync + 'static,
{
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        // Preserve the caller's existing behaviour for anything that is not a
        // plain file; do not cache a decision we would not have made anyway.
        return crate::fs::read_yaml(path);
    }
    let identity = FileIdentity::of(&metadata);
    if let Ok(guard) = cache().lock()
        && let Some(entry) = guard.get(path)
        && entry.identity == identity
        && let Some(value) = entry.value.downcast_ref::<T>()
    {
        return Ok(value.clone());
    }
    let value: T = crate::fs::read_yaml(path)?;
    if let Ok(mut guard) = cache().lock() {
        guard.insert(
            path.to_path_buf(),
            Entry {
                identity,
                value: Box::new(value.clone()),
            },
        );
    }
    Ok(value)
}

/// Record the parse of a document the caller just wrote, so the read-after-write
/// path does not have to go back to disk at all.
pub fn store_written<T>(path: &Path, value: &T)
where
    T: Clone + Send + Sync + 'static,
{
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        invalidate(path);
        return;
    };
    let identity = FileIdentity::of(&metadata);
    if let Ok(mut guard) = cache().lock() {
        guard.insert(
            path.to_path_buf(),
            Entry {
                identity,
                value: Box::new(value.clone()),
            },
        );
    }
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
