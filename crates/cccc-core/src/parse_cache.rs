//! Decode reuse for editable YAML. Every load still reads the complete file.

#[cfg(not(unix))]
pub(crate) use crate::fs::read_yaml;
#[cfg(unix)]
pub(crate) use cached::{invalidate, read_yaml};

#[cfg(not(unix))]
pub(crate) fn invalidate(_path: &std::path::Path) {}

#[cfg(unix)]
mod cached {
    use same_file::Handle;
    use serde::de::DeserializeOwned;
    use std::any::{Any, TypeId};
    use std::collections::HashMap;
    use std::fs::{self, File};
    use std::io::{self, Read};
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, OnceLock};

    const MAX_FILE_BYTES: usize = 256 * 1024;
    const MAX_ENTRIES: usize = 64;
    const MAX_WEIGHT: usize = 8 * 1024 * 1024;
    type Key = (PathBuf, TypeId);

    struct Entry {
        identity: Handle,
        bytes: Vec<u8>,
        value: Arc<dyn Any + Send + Sync>,
        weight: usize,
        used: u64,
    }

    #[derive(Default)]
    struct Cache {
        entries: HashMap<Key, Entry>,
        clock: u64,
        weight: usize,
    }

    impl Cache {
        fn get(
            &mut self,
            key: &Key,
            identity: &Handle,
            bytes: &[u8],
        ) -> Option<Arc<dyn Any + Send + Sync>> {
            let entry = self.entries.get_mut(key)?;
            if entry.identity != *identity || entry.bytes != bytes {
                return None;
            }
            self.clock = self.clock.wrapping_add(1);
            entry.used = self.clock;
            Some(Arc::clone(&entry.value))
        }

        // Return retired values so deep drops and descriptor closes happen outside the mutex.
        fn insert(&mut self, key: Key, mut entry: Entry) -> Vec<Entry> {
            self.clock = self.clock.wrapping_add(1);
            entry.used = self.clock;
            self.weight = self.weight.saturating_add(entry.weight);
            let mut retired = Vec::new();
            if let Some(old) = self.entries.insert(key, entry) {
                self.weight = self.weight.saturating_sub(old.weight);
                retired.push(old);
            }
            while self.entries.len() > MAX_ENTRIES || self.weight > MAX_WEIGHT {
                let Some(key) = self
                    .entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.used)
                    .map(|(key, _)| key.clone())
                else {
                    break;
                };
                if let Some(old) = self.entries.remove(&key) {
                    self.weight = self.weight.saturating_sub(old.weight);
                    retired.push(old);
                }
            }
            retired
        }

        fn invalidate(&mut self, path: &Path) -> Vec<Entry> {
            let keys: Vec<_> = self
                .entries
                .keys()
                .filter(|(candidate, _)| candidate == path)
                .cloned()
                .collect();
            keys.into_iter()
                .filter_map(|key| {
                    let entry = self.entries.remove(&key)?;
                    self.weight = self.weight.saturating_sub(entry.weight);
                    Some(entry)
                })
                .collect()
        }
    }

    fn cache() -> &'static Mutex<Cache> {
        static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
        CACHE.get_or_init(Mutex::default)
    }

    pub(crate) fn invalidate(path: &Path) {
        let retired = cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .invalidate(path);
        drop(retired);
    }

    pub(crate) fn read_yaml<T>(path: &Path) -> io::Result<T>
    where
        T: DeserializeOwned + Clone + Send + Sync + 'static,
    {
        let before = fs::symlink_metadata(path)?;
        let mut file = File::open(path)?;
        let opened = file.metadata()?;
        if !before.is_file()
            || !opened.is_file()
            || before.dev() != opened.dev()
            || before.ino() != opened.ino()
            || opened.len() > MAX_FILE_BYTES as u64
        {
            return serde_yaml::from_reader(file).map_err(io::Error::other);
        }
        let identity = match file.try_clone().and_then(Handle::from_file) {
            Ok(identity) => identity,
            Err(_) => return serde_yaml::from_reader(file).map_err(io::Error::other),
        };
        let mut bytes = Vec::with_capacity(opened.len() as usize);
        (&mut file)
            .take((MAX_FILE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_FILE_BYTES {
            return serde_yaml::from_reader(io::Cursor::new(bytes).chain(file))
                .map_err(io::Error::other);
        }
        let key = (path.to_path_buf(), TypeId::of::<T>());
        let hit = cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key, &identity, &bytes);
        if let Some(value) = hit.and_then(|value| value.downcast::<T>().ok()) {
            // Only Arc cloning occurs under the mutex; caller-owned deep clones do not.
            return Ok((*value).clone());
        }
        let value: T = serde_yaml::from_slice(&bytes).map_err(io::Error::other)?;
        // An estimate for slot eviction, not a process-memory ceiling.
        let weight = bytes
            .len()
            .saturating_mul(16)
            .saturating_add(std::mem::size_of::<T>());
        let entry = Entry {
            identity,
            bytes,
            value: Arc::new(value.clone()),
            weight,
            used: 0,
        };
        let retired = cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, entry);
        drop(retired);
        Ok(value)
    }

    #[cfg(test)]
    mod tests {
        include!("parse_cache_tests.rs");
    }
}
