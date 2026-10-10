use super::LedgerIndex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

const MAX_ENTRIES: usize = 32;
const MAX_SOURCE_BYTES: u64 = 128 * 1024 * 1024;
// An index not touched for this long forfeits its cache slot even while a
// reader still borrows it: the borrower keeps its Arc and finishes on its own
// copy, so correctness is unaffected — only the shared fast path is dropped.
// Without this, overlapping readers pin every entry (`strong_count == 1` is
// never true mid-storm) and the cache retains every decoded ledger at once.
const IDLE_TTL: Duration = Duration::from_secs(60);

struct CacheEntry {
    index: Arc<RwLock<LedgerIndex>>,
    source_bytes: u64,
    last_used: Instant,
}

struct IndexCache {
    entries: HashMap<PathBuf, CacheEntry>,
    source_bytes: u64,
    idle_ttl: Duration,
    max_entries: usize,
    max_source_bytes: u64,
}

impl IndexCache {
    fn new(max_entries: usize, max_source_bytes: u64) -> Self {
        Self {
            entries: HashMap::new(),
            source_bytes: 0,
            idle_ttl: IDLE_TTL,
            max_entries,
            max_source_bytes,
        }
    }

    fn entry(
        &mut self,
        path: &Path,
        source_bytes: u64,
    ) -> (Arc<RwLock<LedgerIndex>>, Vec<CacheEntry>) {
        let now = Instant::now();
        let index = if let Some(cached) = self.entries.get_mut(path) {
            let source_bytes = source_bytes.max(cached.source_bytes);
            self.source_bytes = self
                .source_bytes
                .saturating_sub(cached.source_bytes)
                .saturating_add(source_bytes);
            cached.source_bytes = source_bytes;
            cached.last_used = now;
            Arc::clone(&cached.index)
        } else {
            let index = Arc::new(RwLock::new(LedgerIndex::default()));
            self.entries.insert(
                path.to_path_buf(),
                CacheEntry {
                    index: Arc::clone(&index),
                    source_bytes,
                    last_used: now,
                },
            );
            self.source_bytes = self.source_bytes.saturating_add(source_bytes);
            index
        };
        let retired = self.evict(path, now);
        (index, retired)
    }

    fn get(&mut self, path: &Path) -> Option<Arc<RwLock<LedgerIndex>>> {
        let cached = self.entries.get_mut(path)?;
        cached.last_used = Instant::now();
        Some(Arc::clone(&cached.index))
    }

    fn update_weight(
        &mut self,
        path: &Path,
        source_bytes: u64,
        expected: &Arc<RwLock<LedgerIndex>>,
    ) -> Vec<CacheEntry> {
        let now = Instant::now();
        let Some(cached) = self.entries.get_mut(path) else {
            return Vec::new();
        };
        if !Arc::ptr_eq(&cached.index, expected) {
            return Vec::new();
        }
        self.source_bytes = self
            .source_bytes
            .saturating_sub(cached.source_bytes)
            .saturating_add(source_bytes);
        cached.source_bytes = source_bytes;
        cached.last_used = now;
        self.evict(path, now)
    }

    fn remove(&mut self, path: &Path) -> Option<CacheEntry> {
        let removed = self.entries.remove(path)?;
        self.source_bytes = self.source_bytes.saturating_sub(removed.source_bytes);
        Some(removed)
    }

    fn evict(&mut self, protected: &Path, now: Instant) -> Vec<CacheEntry> {
        let mut retired: Vec<CacheEntry> = self
            .entries
            .keys()
            .filter(|path| {
                path.as_path() != protected
                    && now.duration_since(self.entries[path.as_path()].last_used) > self.idle_ttl
            })
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|path| self.remove(&path))
            .collect();
        while self.entries.len() > self.max_entries
            || (self.source_bytes > self.weight_limit() && self.entries.len() > 1)
        {
            // Soft evictions prefer unborrowed entries, but a borrow storm can
            // hold every slot at once. Past the hard ceiling a borrowed entry
            // must go too — its reader finishes on its own Arc.
            let hard = self.source_bytes > self.weight_limit().saturating_mul(2);
            let candidate = self
                .entries
                .iter()
                .filter(|(path, cached)| {
                    path.as_path() != protected && (hard || Arc::strong_count(&cached.index) == 1)
                })
                .min_by_key(|(_, cached)| cached.last_used)
                .map(|(path, _)| path.clone());
            let Some(candidate) = candidate else { break };
            if let Some(removed) = self.remove(&candidate) {
                retired.push(removed);
            }
        }
        retired
    }

    fn weight_limit(&self) -> u64 {
        let oversized_allowance = self
            .entries
            .values()
            .map(|cached| cached.source_bytes)
            .max()
            .filter(|largest| *largest > self.max_source_bytes)
            .unwrap_or_default();
        self.max_source_bytes.saturating_add(oversized_allowance)
    }
}

fn cache() -> &'static Mutex<IndexCache> {
    static CACHE: OnceLock<Mutex<IndexCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(IndexCache::new(MAX_ENTRIES, MAX_SOURCE_BYTES)))
}

pub(super) fn entry(path: &Path, source_bytes: u64) -> Arc<RwLock<LedgerIndex>> {
    let (index, retired) = cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(path, source_bytes);
    // Large indexes can contain millions of allocations. Free them after the
    // cache lock is released so unrelated Groups can keep reading.
    drop(retired);
    index
}

pub(super) fn get(path: &Path) -> Option<Arc<RwLock<LedgerIndex>>> {
    cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(path)
}

pub(super) fn update_weight(path: &Path, source_bytes: u64, expected: &Arc<RwLock<LedgerIndex>>) {
    let retired = cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .update_weight(path, source_bytes, expected);
    drop(retired);
}

pub(super) fn invalidate(path: &Path) {
    let retired = cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(path);
    drop(retired);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_least_recently_used_entries_by_count_and_weight() {
        let mut cache = IndexCache::new(2, 10);
        let first = Path::new("first");
        let second = Path::new("second");
        let third = Path::new("third");
        cache.entry(first, 4);
        cache.entry(second, 4);
        cache.get(first);
        cache.entry(third, 4);
        assert!(cache.entries.contains_key(first));
        assert!(!cache.entries.contains_key(second));
        assert!(cache.entries.contains_key(third));
        assert!(cache.source_bytes <= 10);
    }

    #[test]
    fn retains_one_oversized_entry_alongside_budgeted_smaller_entries() {
        let mut cache = IndexCache::new(2, 10);
        let path = Path::new("oversized");
        let active = cache.entry(path, 20).0;
        cache.update_weight(path, 20, &active);
        cache.entry(Path::new("small"), 2);

        assert_eq!(cache.entries.len(), 2);
        assert!(cache.entries.contains_key(path));
        assert!(cache.entries.contains_key(Path::new("small")));
        assert_eq!(cache.source_bytes, 22);
        assert_eq!(Arc::strong_count(&active), 2);
    }

    #[test]
    fn replaces_the_older_oversized_entry() {
        let mut cache = IndexCache::new(4, 10);
        cache.entry(Path::new("old-large"), 20);
        cache.entry(Path::new("small"), 2);
        cache.entry(Path::new("new-large"), 30);

        assert!(!cache.entries.contains_key(Path::new("old-large")));
        assert!(cache.entries.contains_key(Path::new("small")));
        assert!(cache.entries.contains_key(Path::new("new-large")));
        assert_eq!(cache.source_bytes, 32);
    }

    #[test]
    fn updates_weight_and_evicts_the_idle_lru_entry() {
        let mut cache = IndexCache::new(2, 10);
        let first = Path::new("first");
        let second = Path::new("second");
        drop(cache.entry(first, 4));
        drop(cache.entry(second, 4));
        let first_index = cache.get(first).expect("first entry");

        cache.update_weight(first, 9, &first_index);

        assert!(cache.entries.contains_key(first));
        assert!(!cache.entries.contains_key(second));
        assert_eq!(cache.source_bytes, 9);
    }

    #[test]
    fn defers_eviction_while_an_entry_is_in_use() {
        let mut cache = IndexCache::new(1, 10);
        let first = Path::new("first");
        let second = Path::new("second");
        let active = cache.entry(first, 4).0;
        let second_index = cache.entry(second, 4).0;
        assert_eq!(cache.entries.len(), 2);

        drop(active);
        drop(cache.entry(second, 4));

        assert!(!cache.entries.contains_key(first));
        assert!(cache.entries.contains_key(second));
        assert_eq!(Arc::strong_count(&second_index), 2);
    }

    #[test]
    fn retires_idle_entries_even_while_borrowed() {
        let mut cache = IndexCache::new(8, u64::MAX);
        let stale = Path::new("stale");
        let borrowed = cache.entry(stale, 4).0;
        cache.entries.get_mut(stale).expect("entry").last_used =
            Instant::now() - cache.idle_ttl - Duration::from_secs(1);
        // A concurrent reader still holds the entry: the borrow gate must not
        // keep it cached past its idle window.
        assert!(Arc::strong_count(&borrowed) > 1);
        cache.entry(Path::new("fresh"), 4);
        assert!(!cache.entries.contains_key(stale));
        // The borrower finishes on its own Arc; only the shared slot is gone.
        assert_eq!(Arc::strong_count(&borrowed), 1);
    }

    #[test]
    fn keeps_borrowed_entries_that_are_still_hot() {
        let mut cache = IndexCache::new(1, 10);
        let hot = Path::new("hot");
        let borrowed = cache.entry(hot, 4).0;
        cache.entry(Path::new("other"), 4);
        // Borrowed but touched inside the idle window: eviction still defers.
        assert!(cache.entries.contains_key(hot));
        assert_eq!(Arc::strong_count(&borrowed), 2);
    }

    #[test]
    fn retires_borrowed_entries_past_the_hard_ceiling() {
        let mut cache = IndexCache::new(8, 10);
        // weight_limit() == 10 here, so the hard ceiling is 20.
        let a = cache.entry(Path::new("a"), 8).0;
        let b = cache.entry(Path::new("b"), 8).0;
        // 16 weighted bytes is over budget but under the hard ceiling: the
        // borrow gate still protects mid-read entries.
        assert!(cache.entries.contains_key(Path::new("a")));
        assert!(cache.entries.contains_key(Path::new("b")));
        let c = cache.entry(Path::new("c"), 8).0;
        // 24 > 20: the LRU borrowed entry must go even though a reader holds
        // it; the reader finishes on its own Arc.
        assert!(!cache.entries.contains_key(Path::new("a")));
        assert_eq!(Arc::strong_count(&a), 1);
        assert!(cache.entries.contains_key(Path::new("b")));
        assert!(cache.entries.contains_key(Path::new("c")));
        assert_eq!(Arc::strong_count(&b), 2);
        assert_eq!(Arc::strong_count(&c), 2);
        assert_eq!(cache.source_bytes, 16);
    }
}
