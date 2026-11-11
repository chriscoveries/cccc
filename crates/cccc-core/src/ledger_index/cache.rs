use super::LedgerIndex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

const MAX_ENTRIES: usize = 32;
const MAX_RETAINED_BYTES: u64 = 128 * 1024 * 1024;

struct Budget {
    used: AtomicU64,
    limit: u64,
}

impl Budget {
    fn reserve(self: &Arc<Self>, bytes: u64) -> Option<Permit> {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|next| *next <= self.limit)
            })
            .ok()?;
        Some(Permit {
            budget: Arc::clone(self),
            bytes,
        })
    }
}

struct Permit {
    budget: Arc<Budget>,
    bytes: u64,
}
impl Drop for Permit {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

pub(super) struct CachedIndex {
    pub index: LedgerIndex,
    // The permit follows the Arc, including readers of retired entries.
    _permit: Option<Permit>,
}

struct Entry {
    index: Arc<CachedIndex>,
    last_used: u64,
}
struct IndexCache {
    entries: HashMap<PathBuf, Entry>,
    clock: u64,
    max_entries: usize,
    budget: Arc<Budget>,
}

impl IndexCache {
    fn new(max_entries: usize, max_bytes: u64) -> Self {
        Self {
            entries: HashMap::new(),
            clock: 0,
            max_entries,
            budget: Arc::new(Budget {
                used: AtomicU64::new(0),
                limit: max_bytes,
            }),
        }
    }
    fn get(&mut self, path: &Path) -> Option<Arc<CachedIndex>> {
        self.clock = self.clock.wrapping_add(1);
        let entry = self.entries.get_mut(path)?;
        entry.last_used = self.clock;
        Some(Arc::clone(&entry.index))
    }
    fn retire_for(&mut self, path: &Path, bytes: u64) -> Vec<Entry> {
        if bytes > self.budget.limit {
            return Vec::new();
        }
        let mut retired = Vec::new();
        let mut reclaimed = 0;
        if let Some(entry) = self.entries.remove(path) {
            reclaimed += reclaimable(&entry);
            retired.push(entry);
        }
        // Retirement may not free borrowed entries. Reservation below is the
        // authority; no oversized allowance or borrowed-entry exemption.
        while !self.entries.is_empty()
            && (self.entries.len() >= self.max_entries
                || self
                    .budget
                    .used
                    .load(Ordering::Acquire)
                    .saturating_sub(reclaimed)
                    .saturating_add(bytes)
                    > self.budget.limit)
        {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(path, _)| path.clone())
                .expect("nonempty");
            let entry = self.entries.remove(&oldest).expect("oldest");
            reclaimed += reclaimable(&entry);
            retired.push(entry);
        }
        retired
    }
}

fn reclaimable(entry: &Entry) -> u64 {
    if Arc::strong_count(&entry.index) == 1 {
        entry
            .index
            ._permit
            .as_ref()
            .map_or(0, |permit| permit.bytes)
    } else {
        0
    }
}

fn cache() -> &'static Mutex<IndexCache> {
    static CACHE: OnceLock<Mutex<IndexCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(IndexCache::new(MAX_ENTRIES, MAX_RETAINED_BYTES)))
}

pub(super) fn get(path: &Path) -> Option<Arc<CachedIndex>> {
    cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(path)
}

pub(super) fn admit(path: &Path, index: LedgerIndex) -> Arc<CachedIndex> {
    let bytes = index.weight();
    let (budget, retired) = {
        let mut cache = cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = cache.entries.get(path)
            && entry.index.index.sources == index.sources
        {
            return Arc::clone(&entry.index);
        }
        (Arc::clone(&cache.budget), cache.retire_for(path, bytes))
    };
    // Free large tables outside the global lock, before trying to reserve.
    drop(retired);
    let permit = budget.reserve(bytes);
    let retained = permit.is_some();
    let entry = Arc::new(CachedIndex {
        index,
        _permit: permit,
    });
    if retained {
        let displaced = {
            let mut cache = cache()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // A simultaneous admission may have filled the entry-count limit.
            // The byte permit still accounts for both entries until drop.
            let retired = cache.retire_for(path, 0);
            cache.clock = cache.clock.wrapping_add(1);
            let last_used = cache.clock;
            cache.entries.insert(
                path.to_owned(),
                Entry {
                    index: Arc::clone(&entry),
                    last_used,
                },
            );
            retired
        };
        drop(displaced);
    }
    // No reservation means an uncached query result, never over-budget cache
    // retention. Its temporary metadata lives only for this request.
    entry
}

pub(super) fn invalidate(path: &Path) {
    let retired = cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entries
        .remove(path);
    drop(retired);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_oversized_reservation_is_refused() {
        let budget = Arc::new(Budget {
            used: AtomicU64::new(0),
            limit: 10,
        });
        assert!(budget.reserve(11).is_none());
        assert_eq!(budget.used.load(Ordering::Acquire), 0);
    }

    #[test]
    fn borrowed_retired_entries_remain_charged_until_the_last_reader_drops() {
        let budget = Arc::new(Budget {
            used: AtomicU64::new(0),
            limit: 10,
        });
        let entry = Arc::new(CachedIndex {
            index: LedgerIndex::default(),
            _permit: budget.reserve(8),
        });
        let reader = Arc::clone(&entry);
        drop(entry);
        assert!(budget.reserve(3).is_none());
        assert!(budget.reserve(2).is_some());
        drop(reader);
        assert_eq!(budget.used.load(Ordering::Acquire), 0);
        assert!(budget.reserve(10).is_some());
    }

    #[test]
    fn concurrent_reservations_never_exceed_the_explicit_budget() {
        let budget = Arc::new(Budget {
            used: AtomicU64::new(0),
            limit: 7,
        });
        std::thread::scope(|scope| {
            for _ in 0..16 {
                let budget = Arc::clone(&budget);
                scope.spawn(move || {
                    for _ in 0..100 {
                        let permit = budget.reserve(3);
                        assert!(budget.used.load(Ordering::Acquire) <= budget.limit);
                        std::thread::yield_now();
                        drop(permit);
                    }
                });
            }
        });
        assert_eq!(budget.used.load(Ordering::Acquire), 0);
    }
    fn insert(cache: &mut IndexCache, name: &str, bytes: u64) {
        let path = Path::new(name);
        let retired = cache.retire_for(path, bytes);
        drop(retired);
        let permit = cache.budget.reserve(bytes).expect("within budget");
        cache.clock += 1;
        cache.entries.insert(
            path.to_owned(),
            Entry {
                index: Arc::new(CachedIndex {
                    index: LedgerIndex::default(),
                    _permit: Some(permit),
                }),
                last_used: cache.clock,
            },
        );
    }

    #[test]
    fn evicts_least_recently_used_entries_by_count_and_weight() {
        for (count, bytes) in [(2, 100), (32, 10)] {
            let mut cache = IndexCache::new(count, bytes);
            insert(&mut cache, "first", 4);
            insert(&mut cache, "second", 4);
            drop(cache.get(Path::new("first")));
            insert(&mut cache, "third", 4);
            assert!(cache.entries.contains_key(Path::new("first")));
            assert!(!cache.entries.contains_key(Path::new("second")));
            assert!(cache.entries.contains_key(Path::new("third")));
            assert_eq!(cache.budget.used.load(Ordering::Acquire), 8);
        }
    }

    #[test]
    fn oversized_metadata_does_not_flush_smaller_entries() {
        let mut cache = IndexCache::new(2, 10);
        insert(&mut cache, "small", 2);
        assert!(cache.retire_for(Path::new("oversized"), 20).is_empty());
        assert!(cache.budget.reserve(20).is_none());
        assert_eq!(cache.entries.len(), 1);
    }

    #[test]
    fn updating_weight_retires_only_the_idle_lru_entry_needed() {
        let mut cache = IndexCache::new(3, 10);
        insert(&mut cache, "first", 4);
        insert(&mut cache, "second", 4);
        insert(&mut cache, "first", 9);
        assert!(cache.entries.contains_key(Path::new("first")));
        assert!(!cache.entries.contains_key(Path::new("second")));
        assert_eq!(cache.budget.used.load(Ordering::Acquire), 9);
    }

    #[test]
    fn retired_borrowed_entries_cannot_expand_the_budget() {
        let mut cache = IndexCache::new(1, 10);
        insert(&mut cache, "first", 8);
        let reader = cache.get(Path::new("first")).expect("reader");
        drop(cache.retire_for(Path::new("second"), 4));
        assert!(cache.entries.is_empty());
        assert!(cache.budget.reserve(4).is_none());
        drop(reader);
        insert(&mut cache, "second", 4);
        assert_eq!(cache.entries.len(), 1);
    }
}
